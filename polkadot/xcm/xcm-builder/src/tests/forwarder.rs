// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use crate::{
	AccountId32Aliases, AllowUnpaidExecutionFrom, FixedWeightBounds, FrameTransactionalProcessor,
	FungibleAdapter, IsConcrete, MintLocation, TeleportForwarderForAccountId32,
};
use core::cell::Cell;
use frame_support::{
	construct_runtime, derive_impl, parameter_types,
	traits::{fungible::Inspect, ConstU32, Everything, Get, Nothing},
};
use pallet_accumulate_and_forward::Forwarder;
use sp_runtime::{traits::IdentityLookup, AccountId32, BuildStorage};
use xcm::latest::prelude::*;

type AccountId = AccountId32;
type Block = frame_system::mocking::MockBlock<Test>;

// Mock runtime
construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<AccountId>;
	type AccountData = pallet_balances::AccountData<u128>;
}

parameter_types! {
	pub const ExistentialDeposit: u128 = 1;
}

impl pallet_balances::Config for Test {
	type MaxLocks = ConstU32<0>;
	type Balance = u128;
	type RuntimeEvent = RuntimeEvent;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
	type MaxReserves = ConstU32<0>;
	type ReserveIdentifier = [u8; 8];
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = ();
	type MaxFreezes = ConstU32<0>;
	type DoneSlashHandler = ();
}

// XCM mock config
parameter_types! {
	pub const HereLocation: Location = Location::here();
	pub static UniversalLocation: InteriorLocation = (ByGenesis([0; 32]), Parachain(42)).into();
	pub static AssetHubLocation: Location = Location::new(1, [Parachain(1000)]);
	// The remote beneficiary location does not matter for these tests since the XCM is never
	// delivered — we only test that local storage is rolled back on failure.
	pub const AccumulationInterior: InteriorLocation = Junctions::Here;
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub const BaseXcmWeight: Weight = Weight::from_parts(1_000, 1_000);
	pub const AnyNetwork: Option<NetworkId> = None;
}

thread_local! {
	static ROUTER_SHOULD_FAIL: Cell<bool> = Cell::new(false);
}

/// We use an XCM router whose behaviour is controlled by the `ROUTER_SHOULD_FAIL` thread-local.
/// When the flag is set, `validate` returns `Err(SendError::NotApplicable)`, simulating a delivery
/// failure that occurs after `WithdrawAsset` has already debited the source account.
struct ControllableRouter;
impl SendXcm for ControllableRouter {
	type Ticket = ();

	fn validate(_dest: &mut Option<Location>, _msg: &mut Option<Xcm<()>>) -> SendResult<()> {
		if ROUTER_SHOULD_FAIL.with(|f| f.get()) {
			Err(SendError::NotApplicable)
		} else {
			Ok(((), Assets::new()))
		}
	}

	fn deliver(_: ()) -> Result<XcmHash, SendError> {
		Ok([0u8; 32])
	}
}

/// Perform no teleport tracking by disabling the checking-account mechanism.
struct NoTeleportTracking;
impl Get<Option<(AccountId, MintLocation)>> for NoTeleportTracking {
	fn get() -> Option<(AccountId, MintLocation)> {
		None
	}
}

type DapAssetTransactor = FungibleAdapter<
	Balances,
	IsConcrete<HereLocation>,
	AccountId32Aliases<AnyNetwork, AccountId>,
	AccountId,
	NoTeleportTracking,
>;

struct TestXcmConfig;
impl xcm_executor::Config for TestXcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = ControllableRouter;
	type XcmEventEmitter = ();
	type AssetTransactor = DapAssetTransactor;
	type OriginConverter = ();
	type IsReserve = ();
	// `IsTeleporter` check is skipped in test builds by the executor, so any impl works here.
	type IsTeleporter = Everything;
	type UniversalLocation = UniversalLocation;
	type Barrier = AllowUnpaidExecutionFrom<Everything>;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type Trader = ();
	type ResponseHandler = ();
	type AssetTrap = ();
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = ();
	type PalletInstancesInfo = ();
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = ();
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = Nothing;
	type TransactionalProcessor = FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = ();
}

fn new_test_ext(source: AccountId, balance: u128) -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(source, balance)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();
	t.into()
}

/// Verify that when the XCM router fails (simulating a mid-program failure after `WithdrawAsset`
/// has debited the source account), `forward` rolls back all storage changes via
/// `with_transaction`. Both the source balance and total issuance must remain unchanged.
#[test]
fn forward_rolls_back_balance_and_issuance_on_xcm_failure() {
	let source: AccountId = AccountId32::from([1u8; 32]);
	let initial_balance = 1_000u128;

	new_test_ext(source.clone(), initial_balance).execute_with(|| {
		let initial_issuance = Balances::total_issuance();

		ROUTER_SHOULD_FAIL.with(|f| f.set(true));
		let result = TeleportForwarderForAccountId32::<
			TestXcmConfig,
			AssetHubLocation,
			HereLocation,
			AccumulationInterior,
		>::forward(source.clone(), 500u128);
		ROUTER_SHOULD_FAIL.with(|f| f.set(false));

		assert!(result.is_err(), "expected forward to fail when router fails");
		assert_eq!(
			Balances::balance(&source),
			initial_balance,
			"source balance must be unchanged after XCM failure"
		);
		assert_eq!(
			Balances::total_issuance(),
			initial_issuance,
			"total issuance must be unchanged after XCM failure"
		);
	});
}
