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

//! The relay chain: the real `paras` stack plus `pallet-registrar-relay` and enough XCM to talk
//! to the parachain.
//!
//! The `paras`/`configuration`/`shared`/`origin`/`paras_registrar` half is lifted from
//! `polkadot/runtime/common/src/paras_registrar/mock.rs`; the XCM half from
//! `polkadot/xcm/xcm-simulator/example/src/relay_chain`. `paras_registrar` being the real pallet
//! is the point: it is what proves a registration driven from the parachain actually onboards a
//! para and takes no deposit here.

use alloc::collections::btree_map::BTreeMap;
use frame_support::{
	construct_runtime, derive_impl, parameter_types,
	traits::{ConstU128, Everything, Nothing, ProcessMessage, ProcessMessageError},
	weights::{Weight, WeightMeter},
};
use frame_system::EnsureRoot;
use polkadot_primitives::{Id as ParaId, ValidationCode};
use polkadot_runtime_common::{paras_registrar, traits::OnSwap};
use polkadot_runtime_parachains::{
	configuration,
	inclusion::{AggregateMessageOrigin, UmpQueueId},
	origin, paras, shared,
};
use sp_core::ConstUint;
use sp_runtime::{
	traits::IdentityLookup, transaction_validity::TransactionPriority, AccountId32, Permill,
};
use xcm::latest::prelude::*;
use xcm_builder::{
	AccountId32Aliases, AllowUnpaidExecutionFrom, ChildParachainAsNative,
	ChildSystemParachainAsSuperuser, DescribeAllTerminal, DescribeFamily, EnsureDecodableXcm,
	EnsureXcmOrigin, FixedWeightBounds, FrameTransactionalProcessor, FungibleAdapter,
	HashedDescription, IsConcrete, SignedAccountId32AsNative, SignedToAccountId32,
	SovereignSignedViaLocation,
};
use xcm_executor::XcmExecutor;

use crate::{
	senders::{EnsureRegistrarPara, RelaySendToPara},
	MAX_CODE_SIZE, MAX_HEAD_SIZE,
};

pub type AccountId = AccountId32;
pub type Balance = u128;
pub type BlockNumber = u32;

/// Blocks in a session in this mock, matching `paras_registrar/mock.rs`.
pub const BLOCKS_PER_SESSION: BlockNumber = 3;
pub const MAX_PENDING: u32 = 4;

type Block = frame_system::mocking::MockBlockU32<Runtime>;
type UncheckedExtrinsic = frame_system::mocking::MockUncheckedExtrinsic<Runtime>;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

// `paras::Config` requires these: the PVF pre-check votes are submitted as bare extrinsics.
impl<C> frame_system::offchain::CreateTransactionBase<C> for Runtime
where
	RuntimeCall: From<C>,
{
	type Extrinsic = UncheckedExtrinsic;
	type RuntimeCall = RuntimeCall;
}

impl<C> frame_system::offchain::CreateBare<C> for Runtime
where
	RuntimeCall: From<C>,
{
	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		UncheckedExtrinsic::new_bare(call)
	}
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type Balance = Balance;
	type ExistentialDeposit = ConstU128<1>;
	type AccountStore = System;
}

impl shared::Config for Runtime {
	type DisabledValidators = ();
}

impl origin::Config for Runtime {}

impl configuration::Config for Runtime {
	type WeightInfo = configuration::TestWeightInfo;
}

/// Copied from `polkadot_runtime_common::mock`, which is `#[cfg(test)]` and so not importable.
pub struct TestNextSessionRotation;

impl frame_support::traits::EstimateNextSessionRotation<BlockNumber> for TestNextSessionRotation {
	fn average_session_length() -> BlockNumber {
		10
	}

	fn estimate_current_session_progress(
		_now: BlockNumber,
	) -> (Option<Permill>, frame_support::weights::Weight) {
		(None, Weight::zero())
	}

	fn estimate_next_session_rotation(
		_now: BlockNumber,
	) -> (Option<BlockNumber>, frame_support::weights::Weight) {
		(None, Weight::zero())
	}
}

parameter_types! {
	pub const ParasUnsignedPriority: TransactionPriority = TransactionPriority::max_value();
}

impl paras::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = paras::TestWeightInfo;
	type UnsignedPriority = ParasUnsignedPriority;
	type QueueFootprinter = ();
	type NextSessionRotation = TestNextSessionRotation;
	type OnNewHead = ();
	type AssignCoretime = ();
	type Fungible = Balances;
	type CooldownRemovalMultiplier = ConstUint<1>;
	type AuthorizeCurrentCodeOrigin = EnsureRoot<AccountId>;
}

parameter_types! {
	pub static SwapData: BTreeMap<ParaId, u64> = BTreeMap::new();
}

pub struct MockSwap;
impl OnSwap for MockSwap {
	fn on_swap(one: ParaId, other: ParaId) {
		let mut swap_data = SwapData::get();
		let one_data = swap_data.remove(&one).unwrap_or_default();
		let other_data = swap_data.remove(&other).unwrap_or_default();
		swap_data.insert(one, other_data);
		swap_data.insert(other, one_data);
		SwapData::set(swap_data);
	}
}

parameter_types! {
	/// Deliberately non-zero: a local registration still pays, which is what makes
	/// "the parachain-driven path takes nothing here" worth asserting.
	pub const LocalParaDeposit: Balance = 10;
	pub const LocalDataDepositPerByte: Balance = 1;
}

impl paras_registrar::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type OnSwap = MockSwap;
	type ParaDeposit = LocalParaDeposit;
	type DataDepositPerByte = LocalDataDepositPerByte;
	type WeightInfo = paras_registrar::TestWeightInfo;
}

parameter_types! {
	pub const RegistrarUnsignedPriority: TransactionPriority = TransactionPriority::max_value() / 2;
}

impl pallet_registrar_relay::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	// Root, or the one parachain we accept registrations from.
	type ParaOrigin = EnsureRegistrarPara;
	type SendToPara = RelaySendToPara;
	type Registrar = paras_registrar::Pallet<Runtime>;
	type MaxHeadDataSize = frame_support::traits::ConstU32<MAX_HEAD_SIZE>;
	type MaxCodeSize = frame_support::traits::ConstU32<MAX_CODE_SIZE>;
	type MaxPendingRegistrations = frame_support::traits::ConstU32<MAX_PENDING>;
	type UnsignedPriority = RegistrarUnsignedPriority;
	type WeightInfo = ();
}

parameter_types! {
	pub const TokenLocation: Location = Here.into_location();
	pub const RelayNetwork: NetworkId = ByGenesis([0; 32]);
	pub UniversalLocation: InteriorLocation = RelayNetwork::get().into();
	pub const BaseXcmWeight: Weight = Weight::from_parts(1_000, 1_000);
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
}

pub type LocationConverter = (
	HashedDescription<AccountId, DescribeFamily<DescribeAllTerminal>>,
	AccountId32Aliases<RelayNetwork, AccountId>,
);

pub type LocalAssetTransactor =
	FungibleAdapter<Balances, IsConcrete<TokenLocation>, LocationConverter, AccountId, ()>;

/// `ChildParachainAsNative` is the important one: it turns the parachain's `OriginKind::Native`
/// into `origin::Origin::Parachain(id)`, which is what `EnsureRegistrarPara` matches on.
pub type OriginConverter = (
	SovereignSignedViaLocation<LocationConverter, RuntimeOrigin>,
	ChildParachainAsNative<origin::Origin, RuntimeOrigin>,
	SignedAccountId32AsNative<RelayNetwork, RuntimeOrigin>,
	ChildSystemParachainAsSuperuser<ParaId, RuntimeOrigin>,
);

pub type XcmRouter = EnsureDecodableXcm<crate::RelayChainXcmRouter>;
pub type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = ();
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
	type XcmRecorder = XcmPallet;
}

pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>;

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Nothing;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Everything;
	type Weigher = Weigher;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = IsConcrete<TokenLocation>;
	type TrustedLockers = ();
	type SovereignAccountOf = LocationConverter;
	type MaxLockers = frame_support::traits::ConstU32<8>;
	type MaxRemoteLockConsumers = frame_support::traits::ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = pallet_xcm::TestWeightInfo;
	type AdminOrigin = EnsureRoot<AccountId>;
	type AuthorizedAliasConsideration = frame_support::traits::Disabled;
}

parameter_types! {
	/// Generous on purpose. Unlike the `xcm-simulator` example this mock actually runs
	/// `MessageQueue::on_initialize` (via [`run_to_block`]), and with `WeightInfo = ()` the
	/// per-message overhead alone is around 1.2s of ref time, so the example's 1s budget trips
	/// the "not enough weight to service a single message" defensive.
	pub MessageQueueServiceWeight: Weight = Weight::from_parts(100_000_000_000, 10_000_000);
	pub const MessageQueueHeapSize: u32 = 65_536;
	pub const MessageQueueMaxStale: u32 = 16;
}

pub struct MessageProcessor;
impl ProcessMessage for MessageProcessor {
	type Origin = AggregateMessageOrigin;

	fn process_message(
		message: &[u8],
		origin: Self::Origin,
		meter: &mut WeightMeter,
		id: &mut [u8; 32],
	) -> Result<bool, ProcessMessageError> {
		let para = match origin {
			AggregateMessageOrigin::Ump(UmpQueueId::Para(para)) => para,
		};
		xcm_builder::ProcessXcmMessage::<Junction, XcmExecutor<XcmConfig>, RuntimeCall>::process_message(
			message,
			Junction::Parachain(para.into()),
			meter,
			id,
		)
	}
}

impl pallet_message_queue::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Size = u32;
	type HeapSize = MessageQueueHeapSize;
	type MaxStale = MessageQueueMaxStale;
	type ServiceWeight = MessageQueueServiceWeight;
	type IdleMaxServiceWeight = ();
	type MessageProcessor = MessageProcessor;
	type QueueChangeHandler = ();
	type QueuePausedQuery = ();
	type WeightInfo = ();
}

construct_runtime!(
	pub enum Runtime {
		System: frame_system,
		Balances: pallet_balances,
		Configuration: configuration,
		ParasShared: shared,
		Parachains: paras,
		ParasOrigin: origin,
		ParasRegistrar: paras_registrar,
		Registrar: pallet_registrar_relay,
		XcmPallet: pallet_xcm,
		MessageQueue: pallet_message_queue,
	}
);

/// Drive the chain to `n`, rotating sessions every [`BLOCKS_PER_SESSION`] blocks.
///
/// Lifted from `paras_registrar/mock.rs`: onboarding only takes effect on a session boundary, so
/// nothing registers without this.
pub fn run_to_block(n: BlockNumber) {
	System::run_to_block_with::<AllPalletsWithSystem>(
		n,
		frame_system::RunToBlockHooks::default().before_finalize(|bn| {
			if (bn + 1) % BLOCKS_PER_SESSION == 0 {
				let session_index = shared::CurrentSessionIndex::<Runtime>::get() + 1;
				let keys = crate::VALIDATORS.iter().map(|v| v.public().into()).collect();

				shared::Pallet::<Runtime>::set_session_index(session_index);
				shared::Pallet::<Runtime>::set_active_validators_ascending(keys);

				Parachains::test_on_new_session();
			}
		}),
	);
}

/// Advance whole sessions.
pub fn run_to_session(n: BlockNumber) {
	run_to_block(n * BLOCKS_PER_SESSION);
}

/// Have the validators approve `code`, so onboarding can proceed.
///
/// Copied from `polkadot_runtime_common::mock::conclude_pvf_checking`, which is `#[cfg(test)]`
/// and therefore not importable.
pub fn conclude_pvf_checking(validation_code: &ValidationCode, session_index: u32) {
	use polkadot_primitives::PvfCheckStatement;

	let num_required = polkadot_primitives::supermajority_threshold(crate::VALIDATORS.len());
	crate::VALIDATORS.iter().enumerate().take(num_required).for_each(|(idx, key)| {
		let statement = PvfCheckStatement {
			accept: true,
			subject: validation_code.hash(),
			session_index,
			validator_index: (idx as u32).into(),
		};
		let signature = key.sign(&statement.signing_payload());
		let _ = paras::Pallet::<Runtime>::include_pvf_check_statement(
			frame_system::Origin::<Runtime>::None.into(),
			statement,
			signature.into(),
		);
	});
}
