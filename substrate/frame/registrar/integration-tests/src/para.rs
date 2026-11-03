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

//! The control-plane parachain: `pallet-registrar-para` plus enough XCM to talk to the relay.
//!
//! Structured after `polkadot/xcm/xcm-simulator/example/src/parachain`, trimmed to what the
//! registrar needs and with `ParentAsSuperuser` added to the origin converter so the relay
//! chain's `Superuser` reports land as `Root` here.

use frame_support::{
	construct_runtime, derive_impl, parameter_types,
	traits::{
		fungible::HoldConsideration, ConstU128, ConstU32, ConstantStoragePrice, Disabled,
		Everything, LinearStoragePrice, Nothing,
	},
	weights::Weight,
};
use frame_system::EnsureRoot;
use sp_runtime::{traits::IdentityLookup, AccountId32};
use xcm::latest::prelude::*;
use xcm_builder::{
	AccountId32Aliases, AllowUnpaidExecutionFrom, DescribeAllTerminal, DescribeFamily,
	EnsureDecodableXcm, EnsureXcmOrigin, FixedWeightBounds, FrameTransactionalProcessor,
	FungibleAdapter, HashedDescription, IsConcrete, ParentAsSuperuser, SignedAccountId32AsNative,
	SignedToAccountId32, SovereignSignedViaLocation,
};
use xcm_executor::XcmExecutor;
use xcm_simulator::mock_message_queue;

use crate::{
	senders::{ParaSendToRelay, PARA_ID},
	MAX_CODE_SIZE, MAX_HEAD_SIZE, MIN_CODE_SIZE,
};

pub type AccountId = AccountId32;
pub type Balance = u128;

/// The first para id `pallet-registrar-para::reserve` hands out here.
pub const FIRST_PARA_ID: u32 = 2000;

pub const PARA_DEPOSIT: Balance = 1_000;
pub const PER_BYTE: Balance = 10;
/// How long a manager waits for the relay chain before giving up on a registration.
pub const PENDING_DEADLINE: u64 = 50;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type Balance = Balance;
	type ExistentialDeposit = ConstU128<1>;
	type AccountStore = System;
	type RuntimeHoldReason = RuntimeHoldReason;
}

impl mock_message_queue::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type XcmExecutor = XcmExecutor<XcmConfig>;
}

parameter_types! {
	pub const RelayLocation: Location = Location::parent();
	pub const RelayNetwork: NetworkId = ByGenesis([0; 32]);
	pub UniversalLocation: InteriorLocation =
		[GlobalConsensus(RelayNetwork::get()), Parachain(PARA_ID)].into();
	pub const BaseXcmWeight: Weight = Weight::from_parts(1_000, 1_000);
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
}

pub type LocationConverter = (
	HashedDescription<AccountId, DescribeFamily<DescribeAllTerminal>>,
	AccountId32Aliases<RelayNetwork, AccountId>,
);

pub type LocalAssetTransactor =
	FungibleAdapter<Balances, IsConcrete<RelayLocation>, LocationConverter, AccountId, ()>;

/// Note `ParentAsSuperuser`: it is what turns the relay chain's `OriginKind::Superuser` report
/// into `Root` here, which is what `pallet-registrar-para`'s `RelayOrigin` accepts.
pub type OriginConverter = (
	SovereignSignedViaLocation<LocationConverter, RuntimeOrigin>,
	SignedAccountId32AsNative<RelayNetwork, RuntimeOrigin>,
	ParentAsSuperuser<RuntimeOrigin>,
	pallet_xcm::XcmPassthrough<RuntimeOrigin>,
);

pub type XcmRouter = EnsureDecodableXcm<crate::ParachainXcmRouter<MsgQueue>>;
pub type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = PolkadotXcm;
	type AssetTransactor = LocalAssetTransactor;
	type OriginConverter = OriginConverter;
	type IsReserve = ();
	type IsTeleporter = ();
	type UniversalLocation = UniversalLocation;
	type Barrier = AllowUnpaidExecutionFrom<Everything>;
	type Weigher = Weigher;
	type Trader = ();
	type ResponseHandler = ();
	type AssetTrap = ();
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = ();
	type PalletInstancesInfo = ();
	type FeeManager = ();
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = Nothing;
	type TransactionalProcessor = FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = PolkadotXcm;
}

pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>;

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Nothing;
	type XcmReserveTransferFilter = Everything;
	type Weigher = Weigher;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = ();
	type TrustedLockers = ();
	type SovereignAccountOf = LocationConverter;
	type MaxLockers = ConstU32<8>;
	type MaxRemoteLockConsumers = ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = pallet_xcm::TestWeightInfo;
	type AdminOrigin = EnsureRoot<AccountId>;
	type AuthorizedAliasConsideration = Disabled;
}

parameter_types! {
	pub const ParaDeposit: Balance = PARA_DEPOSIT;
	pub const DataDepositPerByte: Balance = PER_BYTE;
	pub const ReservationHoldReason: RuntimeHoldReason =
		RuntimeHoldReason::Registrar(pallet_registrar_para::HoldReason::ParaIdReservation);
	pub const RegistrationHoldReason: RuntimeHoldReason =
		RuntimeHoldReason::Registrar(pallet_registrar_para::HoldReason::Registration);
}

impl pallet_registrar_para::Config for Runtime {
	type ReservationConsideration = HoldConsideration<
		AccountId,
		Balances,
		ReservationHoldReason,
		ConstantStoragePrice<ParaDeposit, Balance>,
	>;
	type RegistrationConsideration = HoldConsideration<
		AccountId,
		Balances,
		RegistrationHoldReason,
		LinearStoragePrice<ConstU128<0>, DataDepositPerByte, Balance>,
	>;
	type SendToRelay = ParaSendToRelay;
	// The relay chain reports with `OriginKind::Superuser`, which `ParentAsSuperuser` turns into
	// `Root`. Nothing else on this chain can produce a `Root` origin in these tests.
	type RelayOrigin = EnsureRoot<AccountId>;
	type FirstPublicParaId = ConstU32<FIRST_PARA_ID>;
	type MinCodeSize = ConstU32<MIN_CODE_SIZE>;
	type MaxCodeSize = ConstU32<MAX_CODE_SIZE>;
	type MaxHeadDataSize = ConstU32<MAX_HEAD_SIZE>;
	type PendingDeadline = ConstU64<PENDING_DEADLINE>;
	// No parachain-system in this simulator; production runtimes should use
	// `cumulus_pallet_parachain_system::RelaychainDataProvider`.
	type BlockNumberProvider = System;
	type WeightInfo = ();
}

use frame_support::traits::ConstU64;

type Block = frame_system::mocking::MockBlock<Runtime>;

construct_runtime!(
	pub struct Runtime {
		System: frame_system,
		Balances: pallet_balances,
		MsgQueue: mock_message_queue,
		PolkadotXcm: pallet_xcm,
		Registrar: pallet_registrar_para,
	}
);
