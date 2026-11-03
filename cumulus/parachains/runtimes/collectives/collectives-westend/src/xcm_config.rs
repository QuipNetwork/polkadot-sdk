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

use super::{
	AccountId, AllPalletsWithSystem, Balance, Balances, BaseDeliveryFee, FeeAssetId, Fellows,
	ParachainInfo, ParachainSystem, PolkadotXcm, Runtime, RuntimeCall, RuntimeEvent,
	RuntimeHoldReason, RuntimeOrigin, TransactionByteFee, WeightToFee, XcmpQueue,
};
use frame_support::{
	parameter_types,
	traits::{
		fungible::HoldConsideration, tokens::imbalance::ResolveTo, ConstU32, ConstU8, Contains,
		Equals, Everything, LinearStoragePrice, Nothing, PalletInfoAccess,
	},
};
use frame_system::EnsureRoot;
use pallet_collator_selection::StakingPotAccountId;
use pallet_xcm::{AuthorizedAliasers, XcmPassthrough};
use parachains_common::xcm_config::{
	AliasAccountId32FromSiblingSystemChain, AllSiblingSystemParachains, ConcreteAssetFromSystem,
	ParentRelayOrSiblingParachains, RelayOrOtherSystemParachains,
};
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_common::xcm_sender::ExponentialPrice;
use westend_runtime_constants::{system_parachain::ASSET_HUB_ID, xcm as xcm_constants};
use xcm::latest::{prelude::*, WESTEND_GENESIS_HASH};
use xcm_builder::{
	AccountId32Aliases, AliasChildLocation, AliasOriginRootUsingFilter,
	AllowExplicitUnpaidExecutionFrom, AllowHrmpNotificationsFromRelayChain,
	AllowKnownQueryResponses, AllowSubscriptionsFrom, AllowTopLevelPaidExecutionFrom,
	DenyRecursively, DenyReserveTransferToRelayChain, DenyThenTry, DescribeAllTerminal,
	DescribeFamily, EnsureXcmOrigin, FrameTransactionalProcessor, FungibleAdapter,
	HashedDescription, IsConcrete, IsParentsOnly, LocatableAssetId, LocationAsSuperuser,
	OriginToPluralityVoice, ParentAsSuperuser, ParentIsPreset, RelayChainAsNative,
	SendXcmFeeToAccount, SiblingParachainAsNative, SiblingParachainConvertsVia,
	SignedAccountId32AsNative, SignedToAccountId32, SovereignSignedViaLocation, TakeWeightCredit,
	TrailingSetTopicAsId, UsingComponents, WeightInfoBounds, WithComputedOrigin, WithUniqueTopic,
	XcmFeeManagerFromComponents,
};
use xcm_executor::XcmExecutor;

// Re-export
pub use testnet_parachains_constants::westend::locations::GovernanceLocation;

parameter_types! {
	pub const RootLocation: Location = Location::here();
	pub const WndLocation: Location = Location::parent();
	pub const RelayNetwork: Option<NetworkId> = Some(NetworkId::ByGenesis(WESTEND_GENESIS_HASH));
	pub RelayChainOrigin: RuntimeOrigin = cumulus_pallet_xcm::Origin::Relay.into();
	pub UniversalLocation: InteriorLocation =
		[GlobalConsensus(RelayNetwork::get().unwrap()), Parachain(ParachainInfo::parachain_id().into())].into();
	pub FellowshipTreasuryLocation: Location =
		PalletInstance(<crate::FellowshipTreasury as PalletInfoAccess>::index() as u8).into();
	pub FellowshipSalaryLocation: Location =
		PalletInstance(<crate::FellowshipSalary as PalletInfoAccess>::index() as u8).into();
	pub SecretarySalaryLocation: Location =
		PalletInstance(<crate::SecretarySalary as PalletInfoAccess>::index() as u8).into();
	pub AmbassadorSalaryLocation: Location =
		PalletInstance(<crate::AmbassadorSalary as PalletInfoAccess>::index() as u8).into();
	pub CheckingAccount: AccountId = PolkadotXcm::check_account();
	pub AccumulateAccount: AccountId = pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	pub const FellowshipAdminBodyId: BodyId = BodyId::Index(xcm_constants::body::FELLOWSHIP_ADMIN_INDEX);
	pub AssetHub: Location = (Parent, Parachain(ASSET_HUB_ID)).into();
	pub const TreasurerBodyId: BodyId = BodyId::Treasury;
	pub AssetHubUsdtId: AssetId = (PalletInstance(50), GeneralIndex(1984)).into();
	pub UsdtAssetHub: LocatableAssetId = LocatableAssetId {
		location: AssetHub::get(),
		asset_id: AssetHubUsdtId::get(),
	};
	pub WndAssetHub: LocatableAssetId = LocatableAssetId {
		location: AssetHub::get(),
		asset_id: WndLocation::get().into(),
	};
	/// The accumulation account on this chain.
	pub AccumulateForwardLocation: Location = {
		AccountId32 { network: None, id: AccumulateAccount::get().into() }.into()
	};
}

/// Type for specifying how a `Location` can be converted into an `AccountId`. This is used
/// when determining ownership of accounts for asset transacting and when attempting to use XCM
/// `Transact` in order to determine the dispatch Origin.
pub type LocationToAccountId = (
	// The parent (Relay-chain) origin converts to the parent `AccountId`.
	ParentIsPreset<AccountId>,
	// Sibling parachain origins convert to AccountId via the `ParaId::into`.
	SiblingParachainConvertsVia<Sibling, AccountId>,
	// Straight up local `AccountId32` origins just alias directly to `AccountId`.
	AccountId32Aliases<RelayNetwork, AccountId>,
	// Foreign locations alias into accounts according to a hash of their standard description.
	HashedDescription<AccountId, DescribeFamily<DescribeAllTerminal>>,
);

/// Means for transacting the native currency on this chain.#[allow(deprecated)]
pub type FungibleTransactor = FungibleAdapter<
	// Use this currency:
	Balances,
	// Use this currency when it is a fungible asset matching the given location or name:
	IsConcrete<WndLocation>,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We don't track any teleports of `Balances`.
	(),
>;

/// This is the type we use to convert an (incoming) XCM origin into a local `Origin` instance,
/// ready for dispatching a transaction with Xcm's `Transact`. There is an `OriginKind` which can
/// biases the kind of local `Origin` it will become.
pub type XcmOriginToTransactDispatchOrigin = (
	// Governance location can gain root.
	LocationAsSuperuser<Equals<GovernanceLocation>, RuntimeOrigin>,
	// Sovereign account converter; this attempts to derive an `AccountId` from the origin location
	// using `LocationToAccountId` and then turn that into the usual `Signed` origin. Useful for
	// foreign chains who want to have a local sovereign account on this chain which they control.
	SovereignSignedViaLocation<LocationToAccountId, RuntimeOrigin>,
	// Native converter for Relay-chain (Parent) location; will convert to a `Relay` origin when
	// recognised.
	RelayChainAsNative<RelayChainOrigin, RuntimeOrigin>,
	// Native converter for sibling Parachains; will convert to a `SiblingPara` origin when
	// recognised.
	SiblingParachainAsNative<cumulus_pallet_xcm::Origin, RuntimeOrigin>,
	// Superuser converter for the Relay-chain (Parent) location. This will allow it to issue a
	// transaction from the Root origin.
	ParentAsSuperuser<RuntimeOrigin>,
	// Native signed account converter; this just converts an `AccountId32` origin into a normal
	// `RuntimeOrigin::Signed` origin of the same 32-byte value.
	SignedAccountId32AsNative<RelayNetwork, RuntimeOrigin>,
	// Xcm origins can be represented natively under the Xcm pallet's Xcm origin.
	XcmPassthrough<RuntimeOrigin>,
);

parameter_types! {
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	// Fellows pluralistic body.
	pub const FellowsBodyId: BodyId = BodyId::Technical;
}

pub struct LocalPlurality;
impl Contains<Location> for LocalPlurality {
	fn contains(loc: &Location) -> bool {
		matches!(loc.unpack(), (0, [Plurality { .. }]))
	}
}

pub type Barrier = TrailingSetTopicAsId<
	DenyThenTry<
		DenyRecursively<DenyReserveTransferToRelayChain>,
		(
			// Allow local users to buy weight credit.
			TakeWeightCredit,
			// Expected responses are OK.
			AllowKnownQueryResponses<PolkadotXcm>,
			// Allow XCMs with some computed origins to pass through.
			WithComputedOrigin<
				(
					// If the message is one that immediately attempts to pay for execution, then
					// allow it.
					AllowTopLevelPaidExecutionFrom<Everything>,
					// Parent and sibling system parachains get free execution.
					AllowExplicitUnpaidExecutionFrom<
						(
							IsParentsOnly<ConstU8<1>>,
							RelayOrOtherSystemParachains<AllSiblingSystemParachains, Runtime>,
							Equals<GovernanceLocation>,
						),
						CheapTrustedAliasers,
					>,
					// Subscriptions for version tracking are OK.
					AllowSubscriptionsFrom<ParentRelayOrSiblingParachains>,
					// HRMP notifications from the relay chain are OK.
					AllowHrmpNotificationsFromRelayChain,
				),
				UniversalLocation,
				ConstU32<8>,
			>,
		),
	>,
>;

/// Locations that will not be charged fees in the executor,
/// either execution or delivery.
/// We only waive fees for system functions, which these locations represent.
pub type WaivedLocations = (
	RelayOrOtherSystemParachains<AllSiblingSystemParachains, Runtime>,
	Equals<FellowshipTreasuryLocation>,
	Equals<FellowshipSalaryLocation>,
	Equals<SecretarySalaryLocation>,
	Equals<AmbassadorSalaryLocation>,
	Equals<RootLocation>,
	LocalPlurality,
	Equals<AccumulateForwardLocation>,
);

/// Cases where a remote origin is accepted as trusted Teleporter for a given asset:
/// - WND with the parent Relay Chain and sibling parachains.
pub type TrustedTeleporters = ConcreteAssetFromSystem<WndLocation>;

/// Aliasing rules that are pure computation and thus cheap enough to also be evaluated by
/// barriers, before any payment is taken.
///
/// - Allow any origin to alias into a child sub-location (equivalent to DescendOrigin),
/// - Allow same accounts to alias into each other across system chains,
/// - Allow AssetHub root to alias into anything.
///
/// `AuthorizedAliasers` is deliberately NOT part of this: it reads storage, which is more than a
/// barrier is allowed to cost.
pub type CheapTrustedAliasers = (
	AliasChildLocation,
	AliasAccountId32FromSiblingSystemChain,
	AliasOriginRootUsingFilter<AssetHub, Everything>,
);

/// Defines origin aliasing rules for this chain. Used by the executor.
///
/// - Allow all the cheap aliasing rules also used by the barriers,
/// - Allow origins explicitly authorized to alias into target location.
pub type TrustedAliasers = (CheapTrustedAliasers, AuthorizedAliasers<Runtime>);

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = PolkadotXcm;
	type AssetTransactor = FungibleTransactor;
	type OriginConverter = XcmOriginToTransactDispatchOrigin;
	// Collectives does not recognize a reserve location for any asset. Users must teleport WND
	// where allowed (e.g. with the Relay Chain).
	type IsReserve = ();
	type IsTeleporter = TrustedTeleporters;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::CollectivesWestendXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	// TODO: once DAP allocates collator budgets, redirect XCM execution fees to the accumulation
	// account instead of StakingPot (use crate::DealWithFeesAccumulate as the OnUnbalanced
	// handler).
	type Trader = UsingComponents<
		WeightToFee,
		WndLocation,
		AccountId,
		Balances,
		ResolveTo<StakingPotAccountId<Runtime>, Balances>,
	>;
	type ResponseHandler = PolkadotXcm;
	type AssetTrap = PolkadotXcm;
	type SubscriptionService = PolkadotXcm;
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type AssetLocker = ();
	type AssetExchanger = ();
	type FeeManager = XcmFeeManagerFromComponents<
		WaivedLocations,
		SendXcmFeeToAccount<Self::AssetTransactor, AccumulateAccount>,
	>;
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = TrustedAliasers;
	type TransactionalProcessor = FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = PolkadotXcm;
}

/// Converts a local signed origin into an XCM location. Forms the basis for local origins
/// sending/executing XCMs.
pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>;

pub type PriceForParentDelivery =
	ExponentialPrice<FeeAssetId, BaseDeliveryFee, TransactionByteFee, ParachainSystem>;

/// The means for routing XCM messages which are not for local execution into the right message
/// queues.
pub type XcmRouter = WithUniqueTopic<(
	// Two routers - use UMP to communicate with the relay chain:
	cumulus_primitives_utility::ParentAsUmp<ParachainSystem, PolkadotXcm, PriceForParentDelivery>,
	// ..and XCMP to communicate with the sibling chains.
	XcmpQueue,
)>;

#[cfg(feature = "runtime-benchmarks")]
parameter_types! {
	pub ReachableDest: Option<Location> = Some(Parent.into());
}

/// Type to convert the Fellows origin to a Plurality `Location` value.
pub type FellowsToPlurality = OriginToPluralityVoice<RuntimeOrigin, Fellows, FellowsBodyId>;

parameter_types! {
	pub const DepositPerItem: Balance = crate::deposit(1, 0);
	pub const DepositPerByte: Balance = crate::deposit(0, 1);
	pub const AuthorizeAliasHoldReason: RuntimeHoldReason = RuntimeHoldReason::PolkadotXcm(pallet_xcm::HoldReason::AuthorizeAlias);
}

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	// We only allow the Fellows to send messages.
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, FellowsToPlurality>;
	type XcmRouter = XcmRouter;
	// We support local origins dispatching XCM executions.
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Nothing; // This parachain is not meant as a reserve location.
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::CollectivesWestendXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = ();
	type TrustedLockers = ();
	type SovereignAccountOf = LocationToAccountId;
	type MaxLockers = ConstU32<8>;
	type WeightInfo = crate::weights::pallet_xcm::WeightInfo<Runtime>;
	type AdminOrigin = EnsureRoot<AccountId>;
	type MaxRemoteLockConsumers = ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	// xcm_executor::Config::Aliasers also uses pallet_xcm::AuthorizedAliasers.
	type AuthorizedAliasConsideration = HoldConsideration<
		AccountId,
		Balances,
		AuthorizeAliasHoldReason,
		LinearStoragePrice<DepositPerItem, DepositPerByte, Balance>,
	>;
}

impl cumulus_pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type XcmExecutor = XcmExecutor<XcmConfig>;
}
