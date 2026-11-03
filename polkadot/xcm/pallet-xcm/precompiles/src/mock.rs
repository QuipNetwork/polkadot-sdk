// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.
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

pub use core::cell::RefCell;
use frame_support::{
	derive_impl, parameter_types,
	traits::{
		fungible::HoldConsideration, AsEnsureOriginWithArg, ConstU32, Equals, Everything,
		EverythingBut, Footprint, Nothing,
	},
	weights::Weight,
};
use frame_system::EnsureRoot;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_runtime::{
	traits::{Convert, Identity, IdentityLookup, TryConvertInto},
	AccountId32, BuildStorage,
};
use xcm::prelude::*;
use xcm_builder::{
	AccountId32Aliases, AllowKnownQueryResponses, AllowSubscriptionsFrom,
	AllowTopLevelPaidExecutionFrom, ChildParachainConvertsVia, DescribeAllTerminal,
	EnsureDecodableXcm, FixedWeightBounds, FungibleAdapter, FungiblesAdapter, HashedDescription,
	IsConcrete, MatchedConvertedConcreteId, NoChecking, TakeWeightCredit,
};
use xcm_executor::XcmExecutor;
use xcm_simulator::helpers::derive_topic_id;

use crate::XcmPrecompile;

pub type AccountId = AccountId32;
pub type Balance = u128;
type Block = frame_system::mocking::MockBlock<Test>;

pub const ALICE: AccountId32 = AccountId::new([0u8; 32]);

parameter_types! {
	pub const MinimumPeriod: u64 = 1;
}

impl pallet_timestamp::Config for Test {
	type Moment = u64;
	type OnTimestampSet = ();
	type MinimumPeriod = MinimumPeriod;
	type WeightInfo = ();
}

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		AssetsPallet: pallet_assets,
		Balances: pallet_balances,
		XcmPallet: pallet_xcm,
		Revive: pallet_revive,
		Timestamp: pallet_timestamp,
	}
);

thread_local! {
	pub static SENT_XCM: RefCell<Vec<(Location, Xcm<()>)>> = RefCell::new(Vec::new());
	pub static FAIL_SEND_XCM: RefCell<bool> = RefCell::new(false);
}
pub(crate) fn sent_xcm() -> Vec<(Location, Xcm<()>)> {
	SENT_XCM.with(|q| (*q.borrow()).clone())
}
/// Sender that never returns error.
pub struct TestSendXcm;
impl SendXcm for TestSendXcm {
	type Ticket = (Location, Xcm<()>);
	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<(Location, Xcm<()>)> {
		if FAIL_SEND_XCM.with(|q| *q.borrow()) {
			return Err(SendError::Transport("Intentional send failure used in tests"));
		}
		let pair = (dest.take().unwrap(), msg.take().unwrap());
		Ok((pair, Assets::new()))
	}
	fn deliver(pair: (Location, Xcm<()>)) -> Result<XcmHash, SendError> {
		let message = pair.1.clone();
		if message
			.iter()
			.any(|instr| matches!(instr, ExpectError(Some((1, XcmError::Unimplemented)))))
		{
			return Err(SendError::Transport("Intentional deliver failure used in tests".into()));
		}
		let hash = derive_topic_id(&message);
		SENT_XCM.with(|q| q.borrow_mut().push(pair));
		Ok(hash)
	}
}
/// Sender that returns error if `X8` junction and stops routing
pub struct TestSendXcmErrX8;
impl SendXcm for TestSendXcmErrX8 {
	type Ticket = (Location, Xcm<()>);
	fn validate(
		dest: &mut Option<Location>,
		_: &mut Option<Xcm<()>>,
	) -> SendResult<(Location, Xcm<()>)> {
		if dest.as_ref().unwrap().len() == 8 {
			dest.take();
			Err(SendError::Transport("Destination location full"))
		} else {
			Err(SendError::NotApplicable)
		}
	}
	fn deliver(pair: (Location, Xcm<()>)) -> Result<XcmHash, SendError> {
		let hash = derive_topic_id(&pair.1);
		SENT_XCM.with(|q| q.borrow_mut().push(pair));
		Ok(hash)
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

parameter_types! {
	pub ExistentialDeposit: Balance = 1;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = frame_support::traits::VariantCountOf<RuntimeFreezeReason>;
}

#[cfg(feature = "runtime-benchmarks")]
/// Simple conversion of `u32` into an `AssetId` for use in benchmarking.
pub struct XcmBenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_assets::BenchmarkHelper<Location, ()> for XcmBenchmarkHelper {
	fn create_asset_id_parameter(id: u32) -> Location {
		Location::new(1, [Parachain(id)])
	}
	fn create_reserve_id_parameter(_: u32) {}
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config for Test {
	type Balance = Balance;
	type AssetId = Location;
	type AssetIdParameter = Location;
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = XcmBenchmarkHelper;
}

// This child parachain is not configured as trusted reserve or teleport location for any assets.
pub const SOME_PARA_ID: u32 = 2009;

parameter_types! {
	pub const RelayLocation: Location = Here.into_location();
	pub const AnyNetwork: Option<NetworkId> = None;
	pub const BaseXcmWeight: Weight = Weight::from_parts(1_000, 1_000);
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub UniversalLocation: InteriorLocation = GlobalConsensus(ByGenesis([0; 32])).into();
	pub CheckingAccount: AccountId = XcmPallet::check_account();
}

pub type SovereignAccountOf = (
	ChildParachainConvertsVia<ParaId, AccountId>,
	AccountId32Aliases<AnyNetwork, AccountId>,
	HashedDescription<AccountId, DescribeAllTerminal>,
);

pub type ForeignAssetsConvertedConcreteId = MatchedConvertedConcreteId<
	Location,
	Balance,
	// Excludes relay/parent chain currency
	EverythingBut<(Equals<RelayLocation>,)>,
	Identity,
	TryConvertInto,
>;

pub type AssetTransactors = (
	FungibleAdapter<Balances, IsConcrete<RelayLocation>, SovereignAccountOf, AccountId, ()>,
	FungiblesAdapter<
		AssetsPallet,
		ForeignAssetsConvertedConcreteId,
		SovereignAccountOf,
		AccountId,
		NoChecking,
		CheckingAccount,
	>,
);

pub type Barrier = (
	TakeWeightCredit,
	AllowTopLevelPaidExecutionFrom<Everything>,
	AllowKnownQueryResponses<XcmPallet>,
	AllowSubscriptionsFrom<Everything>,
);

pub type XcmRouter = EnsureDecodableXcm<(TestSendXcmErrX8, TestSendXcm)>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type AssetTransactor = AssetTransactors;
	type OriginConverter = ();
	type IsReserve = ();
	type IsTeleporter = ();
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type Trader = ();
	type ResponseHandler = XcmPallet;
	type AssetTrap = XcmPallet;
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = XcmPallet;
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = ();
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = Nothing;
	type TransactionalProcessor = xcm_builder::FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = XcmPallet;
	type XcmEventEmitter = XcmPallet;
}

pub type LocalOriginToLocation = xcm_builder::SignedToAccountId32<RuntimeOrigin, AccountId, ()>;

parameter_types! {
	pub static AdvertisedXcmVersion: xcm::prelude::XcmVersion = 4;
	pub const AuthorizeAliasHoldReason: RuntimeHoldReason = RuntimeHoldReason::XcmPallet(pallet_xcm::HoldReason::AuthorizeAlias);
}

pub struct ConvertDeposit;
impl Convert<Footprint, u128> for ConvertDeposit {
	fn convert(a: Footprint) -> u128 {
		(a.count * 2 + a.size) as u128
	}
}

impl pallet_xcm::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = xcm_builder::EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	type ExecuteXcmOrigin = xcm_builder::EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Nothing;
	type XcmReserveTransferFilter = Everything;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = AdvertisedXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = ();
	type TrustedLockers = ();
	type SovereignAccountOf = ();
	type MaxLockers = ConstU32<8>;
	type MaxRemoteLockConsumers = ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = pallet_xcm::TestWeightInfo;
	type AdminOrigin = EnsureRoot<AccountId>;
	type AuthorizedAliasConsideration =
		HoldConsideration<AccountId, Balances, AuthorizeAliasHoldReason, ConvertDeposit>;
}

#[derive_impl(pallet_revive::config_preludes::TestDefaultConfig)]
impl pallet_revive::Config for Test {
	type AddressMapper = pallet_revive::AccountId32Mapper<Self>;
	type Balance = Balance;
	type Currency = Balances;
	type Precompiles = (XcmPrecompile<Self>,);
	type Time = Timestamp;
	type UploadOrigin = frame_system::EnsureSigned<AccountId>;
	type InstantiateOrigin = frame_system::EnsureSigned<AccountId>;
}

pub(crate) fn buy_execution<C>(fees: impl Into<Asset>) -> Instruction<C> {
	use xcm::latest::prelude::*;
	BuyExecution { fees: fees.into(), weight_limit: Unlimited }
}

pub(crate) fn new_test_ext_with_balances(
	balances: Vec<(AccountId, Balance)>,
) -> sp_io::TestExternalities {
	new_test_ext_with_balances_and_xcm_version(balances, Some(XCM_VERSION), vec![])
}

pub fn new_test_ext_with_balances_and_xcm_version(
	balances: Vec<(AccountId, Balance)>,
	safe_xcm_version: Option<XcmVersion>,
	supported_version: Vec<(Location, XcmVersion)>,
) -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<Test> { balances, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	pallet_xcm::GenesisConfig::<Test> { safe_xcm_version, supported_version, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	pallet_revive::GenesisConfig::<Test> { mapped_accounts: vec![ALICE], ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}
