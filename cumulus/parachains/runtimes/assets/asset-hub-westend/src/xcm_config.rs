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
	AccountId, AllPalletsWithSystem, Assets, Balance, Balances, BaseDeliveryFee, CollatorSelection,
	DepositPerByte, DepositPerItem, FeeAssetId, ForeignAssets, GeneralAdmin, ParachainInfo,
	ParachainSystem, PolkadotXcm, PoolAssets, Runtime, RuntimeCall, RuntimeEvent,
	RuntimeHoldReason, RuntimeOrigin, ToRococoXcmRouter, TransactionByteFee, Uniques, WeightToFee,
	XcmpQueue,
};
use alloc::{collections::BTreeSet, vec, vec::Vec};
use assets_common::{
	matching::{
		IsForeignConcreteAsset, NonTeleportableAssetFromTrustedReserve, ParentLocation,
		TeleportableAssetWithTrustedReserve,
	},
	TrustBackedAssetsAsLocation,
};
use cumulus_primitives_core::{IsSystem, ParaId};
use frame_support::{
	parameter_types,
	traits::{
		fungible::HoldConsideration,
		tokens::imbalance::{ResolveAssetTo, ResolveTo},
		ConstU32, ConstU8, Contains, Equals, Everything, LinearStoragePrice, PalletInfoAccess,
	},
	PalletId,
};
use frame_system::EnsureRoot;
use pallet_xcm::{AuthorizedAliasers, XcmPassthrough};
use parachains_common::xcm_config::{
	AllSiblingSystemParachains, ConcreteAssetFromSystem, RelayOrOtherSystemParachains,
};
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_common::xcm_sender::ExponentialPrice;
use snowbridge_outbound_queue_primitives::v2::exporter::PausableExporter;
use sp_runtime::traits::{AccountIdConversion, TryConvertInto};
use testnet_parachains_constants::westend::{
	accumulate_forward::AccumulateForwardPalletId, locations::AssetHubParaId,
};
use westend_runtime_constants::system_parachain::{
	BRIDGE_HUB_ID, BROKER_ID, COLLECTIVES_ID, PEOPLE_ID,
};
use xcm::latest::{prelude::*, ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH};
use xcm_builder::{
	unique_instances::UniqueInstancesAdapter, AccountId32Aliases, AliasChildLocation,
	AllowExplicitUnpaidExecutionFrom, AllowHrmpNotificationsFromRelayChain,
	AllowKnownQueryResponses, AllowSubscriptionsFrom, AllowTopLevelPaidExecutionFrom,
	DenyRecursively, DenyReserveTransferToRelayChain, DenyThenTry, DescribeAllTerminal,
	DescribeFamily, EnsureXcmOrigin, ExternalConsensusLocationsConverterFor,
	FrameTransactionalProcessor, FungibleAdapter, FungiblesAdapter, HashedDescription, IsConcrete,
	IsParentsOnly, LocalMint, MatchInClassInstances, MatchedConvertedConcreteId, MintLocation,
	NetworkExportTableItem, NoChecking, OriginToPluralityVoice, ParentAsSuperuser, ParentIsPreset,
	RelayChainAsNative, SendXcmFeeToAccount, SiblingParachainAsNative, SiblingParachainConvertsVia,
	SignedAccountId32AsNative, SignedToAccountId32, SingleAssetExchangeAdapter,
	SovereignSignedViaLocation, StartsWith, StartsWithExplicitGlobalConsensus, TakeWeightCredit,
	TrailingSetTopicAsId, UnpaidRemoteExporter, UsingComponents, WeightInfoBounds,
	WithComputedOrigin, WithLatestLocationConverter, WithUniqueTopic, XcmFeeManagerFromComponents,
};
use xcm_executor::XcmExecutor;

parameter_types! {
	pub const RootLocation: Location = Location::here();
	pub const WestendLocation: Location = Location::parent();
	pub const RelayNetwork: Option<NetworkId> = Some(NetworkId::ByGenesis(WESTEND_GENESIS_HASH));
	pub RelayChainOrigin: RuntimeOrigin = cumulus_pallet_xcm::Origin::Relay.into();
	pub UniversalLocation: InteriorLocation =
		[GlobalConsensus(RelayNetwork::get().unwrap()), Parachain(ParachainInfo::parachain_id().into())].into();
	pub UniversalLocationNetworkId: NetworkId = UniversalLocation::get().global_consensus().unwrap();
	pub TrustBackedAssetsPalletLocation: Location =
		PalletInstance(TrustBackedAssetsPalletIndex::get()).into();
	pub TrustBackedAssetsPalletIndex: u8 = <Assets as PalletInfoAccess>::index() as u8;
	pub ForeignAssetsPalletLocation: Location =
		PalletInstance(<ForeignAssets as PalletInfoAccess>::index() as u8).into();
	pub PoolAssetsPalletLocation: Location =
		PalletInstance(<PoolAssets as PalletInfoAccess>::index() as u8).into();
	pub UniquesPalletLocation: Location =
		PalletInstance(<Uniques as PalletInfoAccess>::index() as u8).into();
	pub CheckingAccount: AccountId = PolkadotXcm::check_account();
	pub StakingPot: AccountId = CollatorSelection::account_id();
	/// Asset Hub has mint authority since the Asset Hub migration.
	pub TeleportTracking: Option<(AccountId, MintLocation)> = Some((CheckingAccount::get(), MintLocation::Local));
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
	// Different global consensus locations sovereign accounts.
	ExternalConsensusLocationsConverterFor<UniversalLocation, AccountId>,
);

/// Means for transacting the native currency on this chain.
pub type FungibleTransactor = FungibleAdapter<
	// Use this currency:
	Balances,
	// Use this currency when it is a fungible asset matching the given location or name:
	IsConcrete<WestendLocation>,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// Teleports tracking
	TeleportTracking,
>;

/// `AssetId`/`Balance` converter for `TrustBackedAssets`.
pub type TrustBackedAssetsConvertedConcreteId =
	assets_common::TrustBackedAssetsConvertedConcreteId<TrustBackedAssetsPalletLocation, Balance>;

/// Means for transacting assets besides the native currency on this chain.
pub type FungiblesTransactor = FungiblesAdapter<
	// Use this fungibles implementation:
	Assets,
	// Use this currency when it is a fungible asset matching the given location or name:
	TrustBackedAssetsConvertedConcreteId,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We only want to allow teleports of known assets. We use non-zero issuance as an indication
	// that this asset is known.
	LocalMint<parachains_common::impls::NonZeroIssuance<AccountId, Assets>>,
	// The account to use for tracking teleports.
	CheckingAccount,
>;

/// Matcher for converting `ClassId`/`InstanceId` into a uniques asset.
pub type UniquesConvertedConcreteId =
	assets_common::UniquesConvertedConcreteId<UniquesPalletLocation>;

/// Means for transacting unique assets.
pub type UniquesTransactor = UniqueInstancesAdapter<
	AccountId,
	LocationToAccountId,
	MatchInClassInstances<UniquesConvertedConcreteId>,
	pallet_uniques::asset_ops::Item<Uniques>,
>;

/// `AssetId`/`Balance` converter for `ForeignAssets`.
pub type ForeignAssetsConvertedConcreteId = assets_common::ForeignAssetsConvertedConcreteId<
	(
		// Ignore `TrustBackedAssets` explicitly
		StartsWith<TrustBackedAssetsPalletLocation>,
		// Ignore asset which starts explicitly with our `GlobalConsensus(NetworkId)`, means:
		// - foreign assets from our consensus should be: `Location {parents: 1, X*(Parachain(xyz),
		//   ..)}
		// - foreign assets outside our consensus with the same `GlobalConsensus(NetworkId)` wont
		//   be accepted here
		StartsWithExplicitGlobalConsensus<UniversalLocationNetworkId>,
	),
	Balance,
	xcm::v5::Location,
>;

/// Means for transacting foreign assets from different global consensus.
pub type ForeignFungiblesTransactor = FungiblesAdapter<
	// Use this fungibles implementation:
	ForeignAssets,
	// Use this currency when it is a fungible asset matching the given location or name:
	ForeignAssetsConvertedConcreteId,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We don't need to check teleports here.
	NoChecking,
	// The account to use for tracking teleports.
	CheckingAccount,
>;

/// `AssetId`/`Balance` converter for `PoolAssets`.
pub type PoolAssetsConvertedConcreteId =
	assets_common::PoolAssetsConvertedConcreteId<PoolAssetsPalletLocation, Balance>;

/// Means for transacting asset conversion pool assets on this chain.
pub type PoolFungiblesTransactor = FungiblesAdapter<
	// Use this fungibles implementation:
	PoolAssets,
	// Use this currency when it is a fungible asset matching the given location or name:
	PoolAssetsConvertedConcreteId,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We only want to allow teleports of known assets. We use non-zero issuance as an indication
	// that this asset is known.
	LocalMint<parachains_common::impls::NonZeroIssuance<AccountId, PoolAssets>>,
	// The account to use for tracking teleports.
	CheckingAccount,
>;

parameter_types! {
	/// Taken from the real gas and deposits of a standard ERC20 transfer call.
	pub const ERC20TransferGasLimit: Weight = Weight::from_parts(500_000_000_000, 10 * 1024 * 1024);
	pub const ERC20TransferStorageDepositLimit: Balance = 10_200_000_000;
	pub ERC20TransfersCheckingAccount: AccountId = PalletId(*b"py/revch").into_account_truncating();
	pub DapBufferAccount: AccountId = pallet_dap::Pallet::<Runtime>::buffer_account();
}

/// Transactor for ERC20 tokens.
pub type ERC20Transactor = assets_common::ERC20Transactor<
	// We need this for accessing pallet-revive.
	Runtime,
	// The matcher for smart contracts.
	assets_common::ERC20Matcher,
	// How to convert from a location to an account id.
	LocationToAccountId,
	// The maximum gas that can be used by a standard ERC20 transfer.
	ERC20TransferGasLimit,
	// The maximum storage deposit that can be used by a standard ERC20 transfer.
	ERC20TransferStorageDepositLimit,
	// We're generic over this so we can't escape specifying it.
	AccountId,
	// Checking account for ERC20 transfers.
	ERC20TransfersCheckingAccount,
>;

/// Means for transacting assets on this chain.
pub type AssetTransactors = (
	FungibleTransactor,
	FungiblesTransactor,
	ForeignFungiblesTransactor,
	UniquesTransactor,
	ERC20Transactor,
);

/// This is the type we use to convert an (incoming) XCM origin into a local `Origin` instance,
/// ready for dispatching a transaction with Xcm's `Transact`. There is an `OriginKind` which can
/// biases the kind of local `Origin` it will become.
pub type XcmOriginToTransactDispatchOrigin = (
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
}

pub struct FellowshipEntities;
impl Contains<Location> for FellowshipEntities {
	fn contains(location: &Location) -> bool {
		matches!(
			location.unpack(),
			(1, [Parachain(COLLECTIVES_ID), Plurality { id: BodyId::Technical, .. }]) |
				(1, [Parachain(COLLECTIVES_ID), PalletInstance(64)]) |
				(1, [Parachain(COLLECTIVES_ID), PalletInstance(65)])
		)
	}
}

pub struct LocalPlurality;
impl Contains<Location> for LocalPlurality {
	fn contains(loc: &Location) -> bool {
		matches!(loc.unpack(), (0, [Plurality { .. }]))
	}
}

pub struct AmbassadorEntities;
impl Contains<Location> for AmbassadorEntities {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (1, [Parachain(COLLECTIVES_ID), PalletInstance(74)]))
	}
}

pub struct SecretaryEntities;
impl Contains<Location> for SecretaryEntities {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (1, [Parachain(COLLECTIVES_ID), PalletInstance(91)]))
	}
}

pub struct SystemChainAccumulationAccounts;
impl Contains<Location> for SystemChainAccumulationAccounts {
	fn contains(location: &Location) -> bool {
		let accumulation_account: [u8; 32] =
			AccumulateForwardPalletId::get().into_account_truncating();
		match location.unpack() {
			// Relay chain (parent) accumulation account.
			(1, [AccountId32 { id, .. }]) => *id == accumulation_account,
			// Sibling system parachain accumulation account.
			(1, [Parachain(id), AccountId32 { id: account_id, .. }]) => {
				ParaId::from(*id).is_system() &&
					matches!(*id, BRIDGE_HUB_ID | BROKER_ID | COLLECTIVES_ID | PEOPLE_ID) &&
					*account_id == accumulation_account
			},
			_ => false,
		}
	}
}

pub type Barrier = TrailingSetTopicAsId<
	DenyThenTry<
		DenyRecursively<DenyReserveTransferToRelayChain>,
		(
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
							FellowshipEntities,
							AmbassadorEntities,
							SecretaryEntities,
						),
						CheapTrustedAliasers,
					>,
					// Accumulation accounts get free execution, while aliasing allows the chain
					// (as the XCM sending origin) to claim the identity of these accounts.
					AllowExplicitUnpaidExecutionFrom<
						SystemChainAccumulationAccounts,
						CheapTrustedAliasers,
					>,
					// Subscriptions for version tracking are OK.
					AllowSubscriptionsFrom<Everything>,
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
	Equals<RootLocation>,
	RelayOrOtherSystemParachains<AllSiblingSystemParachains, Runtime>,
	FellowshipEntities,
	AmbassadorEntities,
	LocalPlurality,
	SecretaryEntities,
);

// Asset Hub accepts incoming reserve transfers only for "Foreign Assets" and only from locations
// explicitly set by the asset's owner.
pub type TrustedReserves = (
	IsForeignConcreteAsset<
		NonTeleportableAssetFromTrustedReserve<AssetHubParaId, crate::ForeignAssets>,
	>,
);

/// Cases where a remote origin is accepted as trusted Teleporter for a given asset:
///
/// - WND with the parent Relay Chain and sibling system parachains; and
/// - Sibling parachains' assets according to their configured trusted reserves (teleportable when
///   `Here` and `origin` are both trusted reserve locations).
pub type TrustedTeleporters = (
	ConcreteAssetFromSystem<WestendLocation>,
	IsForeignConcreteAsset<
		TeleportableAssetWithTrustedReserve<AssetHubParaId, crate::ForeignAssets>,
	>,
);

/// Aliasing rules that are pure computation and thus cheap enough to also be evaluated by
/// barriers, before any payment is taken.
///
/// - Allow any origin to alias into a child sub-location (equivalent to DescendOrigin).
///
/// `AuthorizedAliasers` is deliberately NOT part of this: it reads storage, which is more than a
/// barrier is allowed to cost.
pub type CheapTrustedAliasers = (AliasChildLocation,);

/// Defines origin aliasing rules for this chain. Used by the executor.
///
/// - Allow all the cheap aliasing rules also used by the barriers,
/// - Allow origins explicitly authorized by the alias target location.
pub type TrustedAliasers = (CheapTrustedAliasers, AuthorizedAliasers<Runtime>);

/// Asset converter for pool assets.
/// Used to convert one asset to another, when there is a pool available between the two.
/// This type thus allows paying fees with any asset as long as there is a pool between said
/// asset and the asset required for fee payment.
pub type PoolAssetsExchanger = SingleAssetExchangeAdapter<
	crate::AssetConversion,
	crate::NativeAndNonPoolAssets,
	(
		TrustBackedAssetsAsLocation<TrustBackedAssetsPalletLocation, Balance, xcm::v5::Location>,
		ForeignAssetsConvertedConcreteId,
		// `ForeignAssetsConvertedConcreteId` excludes the relay token, so we add it back here.
		MatchedConvertedConcreteId<
			xcm::v5::Location,
			Balance,
			Equals<ParentLocation>,
			WithLatestLocationConverter<xcm::v5::Location>,
			TryConvertInto,
		>,
	),
	AccountId,
>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = PolkadotXcm;
	type AssetTransactor = AssetTransactors;
	type OriginConverter = XcmOriginToTransactDispatchOrigin;
	type IsReserve = TrustedReserves;
	type IsTeleporter = TrustedTeleporters;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::AssetHubWestendXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	// TODO: once DAP allocates collator budgets, redirect XCM execution fees to DAP
	// instead of StakingPot (use crate::Dap as the OnUnbalanced handler).
	type Trader = (
		UsingComponents<
			WeightToFee,
			WestendLocation,
			AccountId,
			Balances,
			ResolveTo<StakingPot, Balances>,
		>,
		cumulus_primitives_utility::SwapFirstAssetTrader<
			WestendLocation,
			crate::AssetConversion,
			WeightToFee,
			crate::NativeAndNonPoolAssets,
			(
				TrustBackedAssetsAsLocation<
					TrustBackedAssetsPalletLocation,
					Balance,
					xcm::v5::Location,
				>,
				ForeignAssetsConvertedConcreteId,
			),
			ResolveAssetTo<StakingPot, crate::NativeAndNonPoolAssets>,
			AccountId,
		>,
	);
	type ResponseHandler = PolkadotXcm;
	type AssetTrap = PolkadotXcm;
	type SubscriptionService = PolkadotXcm;
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type AssetLocker = ();
	type AssetExchanger = PoolAssetsExchanger;
	type FeeManager = XcmFeeManagerFromComponents<
		WaivedLocations,
		SendXcmFeeToAccount<Self::AssetTransactor, DapBufferAccount>,
	>;
	type MessageExporter = ();
	type UniversalAliases =
		(bridging::to_rococo::UniversalAliases, bridging::to_ethereum::UniversalAliases);
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = TrustedAliasers;
	type TransactionalProcessor = FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = PolkadotXcm;
}

parameter_types! {
	// `GeneralAdmin` pluralistic body.
	pub const GeneralAdminBodyId: BodyId = BodyId::Administration;
}

/// Type to convert the `GeneralAdmin` origin to a Plurality `Location` value.
pub type GeneralAdminToPlurality =
	OriginToPluralityVoice<RuntimeOrigin, GeneralAdmin, GeneralAdminBodyId>;

/// Local origins on this chain are allowed to dispatch XCM sends/executions.
pub type LocalOriginToLocation =
	(GeneralAdminToPlurality, SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>);

pub type PriceForParentDelivery =
	ExponentialPrice<FeeAssetId, BaseDeliveryFee, TransactionByteFee, ParachainSystem>;

/// For routing XCM messages which do not cross local consensus boundary.
type LocalXcmRouter = (
	// Two routers - use UMP to communicate with the relay chain:
	cumulus_primitives_utility::ParentAsUmp<ParachainSystem, PolkadotXcm, PriceForParentDelivery>,
	// ..and XCMP to communicate with the sibling chains.
	XcmpQueue,
);

/// The means for routing XCM messages which are not for local execution into the right message
/// queues.
pub type XcmRouter = WithUniqueTopic<(
	LocalXcmRouter,
	// Router which wraps and sends xcm to BridgeHub to be delivered to the Rococo
	// GlobalConsensus
	ToRococoXcmRouter,
	// Router which wraps and sends xcm to BridgeHub to be delivered to the Ethereum
	// GlobalConsensus with a pausable flag, if the flag is set true then the Router is paused
	PausableExporter<
		crate::SnowbridgeSystemFrontend,
		(
			UnpaidRemoteExporter<
				(
					bridging::to_ethereum::EthereumNetworkExportTableV2,
					bridging::to_ethereum::EthereumNetworkExportTableV1,
				),
				XcmpQueue,
				UniversalLocation,
			>,
		),
	>,
)>;

parameter_types! {
	pub Collectives: Location = Location::new(1, [Parachain(COLLECTIVES_ID)]);
	pub const AuthorizeAliasHoldReason: RuntimeHoldReason = RuntimeHoldReason::PolkadotXcm(pallet_xcm::HoldReason::AuthorizeAlias);
}

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Everything;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::AssetHubWestendXcmWeight<RuntimeCall>,
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

/// All configuration related to bridging
pub mod bridging {
	use super::*;
	use assets_common::matching;

	parameter_types! {
		/// Base price of every byte of the Westend -> Rococo message. Can be adjusted via
		/// governance `set_storage` call.
		///
		/// Default value is our estimation of the:
		///
		/// 1) an approximate cost of XCM execution (`ExportMessage` and surroundings) at Westend bridge hub;
		///
		/// 2) the approximate cost of Westend -> Rococo message delivery transaction on Rococo Bridge Hub,
		///    converted into WNDs using 1:1 conversion rate;
		///
		/// 3) the approximate cost of Westend -> Rococo message confirmation transaction on Westend Bridge Hub.
		pub storage XcmBridgeHubRouterBaseFee: Balance =
			bp_bridge_hub_westend::BridgeHubWestendBaseXcmFeeInWnds::get()
				.saturating_add(bp_bridge_hub_rococo::BridgeHubRococoBaseDeliveryFeeInRocs::get())
				.saturating_add(bp_bridge_hub_westend::BridgeHubWestendBaseConfirmationFeeInWnds::get());
		/// Price of every byte of the Westend -> Rococo message. Can be adjusted via
		/// governance `set_storage` call.
		pub storage XcmBridgeHubRouterByteFee: Balance = TransactionByteFee::get();

		pub SiblingBridgeHubParaId: u32 = bp_bridge_hub_westend::BRIDGE_HUB_WESTEND_PARACHAIN_ID;
		pub SiblingBridgeHub: Location = Location::new(1, [Parachain(SiblingBridgeHubParaId::get())]);
		/// Router expects payment with this `AssetId`.
		/// (`AssetId` has to be aligned with `BridgeTable`)
		pub XcmBridgeHubRouterFeeAssetId: AssetId = WestendLocation::get().into();

		pub BridgeTable: Vec<NetworkExportTableItem> =
			Vec::new().into_iter()
			.chain(to_rococo::BridgeTable::get())
			.collect();
	}

	pub type NetworkExportTable = xcm_builder::NetworkExportTable<BridgeTable>;

	pub mod to_rococo {
		use super::*;

		parameter_types! {
			pub SiblingBridgeHubWithBridgeHubRococoInstance: Location = Location::new(
				1,
				[
					Parachain(SiblingBridgeHubParaId::get()),
					PalletInstance(bp_bridge_hub_westend::WITH_BRIDGE_WESTEND_TO_ROCOCO_MESSAGES_PALLET_INDEX)
				]
			);

			pub const RococoNetwork: NetworkId = NetworkId::ByGenesis(ROCOCO_GENESIS_HASH);
			pub RococoEcosystem: Location = Location::new(2, [GlobalConsensus(RococoNetwork::get())]);
			pub RocLocation: Location = Location::new(2, [GlobalConsensus(RococoNetwork::get())]);
			pub AssetHubRococo: Location = Location::new(2, [
				GlobalConsensus(RococoNetwork::get()),
				Parachain(bp_asset_hub_rococo::ASSET_HUB_ROCOCO_PARACHAIN_ID)
			]);

			/// Set up exporters configuration.
			/// `Option<Asset>` represents static "base fee" which is used for total delivery fee calculation.
			pub BridgeTable: Vec<NetworkExportTableItem> = vec![
				NetworkExportTableItem::new(
					RococoNetwork::get(),
					Some(vec![
						AssetHubRococo::get().interior.split_global().expect("invalid configuration for AssetHubRococo").1,
					]),
					SiblingBridgeHub::get(),
					// base delivery fee to local `BridgeHub`
					Some((
						XcmBridgeHubRouterFeeAssetId::get(),
						XcmBridgeHubRouterBaseFee::get(),
					).into())
				)
			];

			/// Universal aliases
			pub UniversalAliases: BTreeSet<(Location, Junction)> = BTreeSet::from_iter(
				vec![
					(SiblingBridgeHubWithBridgeHubRococoInstance::get(), GlobalConsensus(RococoNetwork::get()))
				]
			);
		}

		impl Contains<(Location, Junction)> for UniversalAliases {
			fn contains(alias: &(Location, Junction)) -> bool {
				UniversalAliases::get().contains(alias)
			}
		}

		/// Allow any asset native to the Rococo ecosystem if it comes from Rococo Asset Hub.
		pub type RococoAssetFromAssetHubRococo =
			matching::RemoteAssetFromLocation<StartsWith<RococoEcosystem>, AssetHubRococo>;
	}

	pub mod to_ethereum {
		use super::*;
		use assets_common::matching::FromNetwork;
		use testnet_parachains_constants::westend::snowbridge::{
			EthereumNetwork, INBOUND_QUEUE_PALLET_INDEX_V1, INBOUND_QUEUE_PALLET_INDEX_V2,
		};

		parameter_types! {
			/// User fee for ERC20 token transfer back to Ethereum.
			/// (initially was calculated by test `OutboundQueue::calculate_fees` - ETH/WND 1/400 and fee_per_gas 20 GWEI = 2200698000000 + *25%)
			/// Needs to be more than fee calculated from DefaultFeeConfig FeeConfigRecord in snowbridge:parachain/pallets/outbound-queue/src/lib.rs
			/// Polkadot uses 10 decimals, Kusama,Rococo,Westend 12 decimals.
			pub const DefaultBridgeHubEthereumBaseFee: Balance = 3_833_568_200_000;
			pub const DefaultBridgeHubEthereumBaseFeeV2: Balance = 100_000_000_000;
			pub storage BridgeHubEthereumBaseFee: Balance = DefaultBridgeHubEthereumBaseFee::get();
			pub storage BridgeHubEthereumBaseFeeV2: Balance = DefaultBridgeHubEthereumBaseFeeV2::get();
			pub SiblingBridgeHubWithEthereumInboundQueueV1Instance: Location = Location::new(
				1,
				[
					Parachain(SiblingBridgeHubParaId::get()),
					PalletInstance(INBOUND_QUEUE_PALLET_INDEX_V1)
				]
			);
			pub SiblingBridgeHubWithEthereumInboundQueueV2Instance: Location = Location::new(
				1,
				[
					Parachain(SiblingBridgeHubParaId::get()),
					PalletInstance(INBOUND_QUEUE_PALLET_INDEX_V2)
				]
			);

			/// Set up exporters configuration.
			/// `Option<Asset>` represents static "base fee" which is used for total delivery fee calculation.
			pub EthereumBridgeTableV1: vec::Vec<NetworkExportTableItem> = vec![
				NetworkExportTableItem::new(
					EthereumNetwork::get(),
					Some(vec![Junctions::Here]),
					SiblingBridgeHub::get(),
					Some((
						XcmBridgeHubRouterFeeAssetId::get(),
						BridgeHubEthereumBaseFee::get(),
					).into())
				),
			];

			pub EthereumBridgeTableV2: vec::Vec<NetworkExportTableItem> = vec![
				NetworkExportTableItem::new(
					EthereumNetwork::get(),
					Some(vec![Junctions::Here]),
					SiblingBridgeHub::get(),
					Some((
						XcmBridgeHubRouterFeeAssetId::get(),
						BridgeHubEthereumBaseFeeV2::get(),
					).into())
				),
			];

			/// Universal aliases
			pub UniversalAliases: BTreeSet<(Location, Junction)> = BTreeSet::from_iter(
				vec![
					(SiblingBridgeHubWithEthereumInboundQueueV2Instance::get(), GlobalConsensus(EthereumNetwork::get().into())),
					(SiblingBridgeHubWithEthereumInboundQueueV1Instance::get(), GlobalConsensus(EthereumNetwork::get().into())),
				]
			);
		}

		pub type EthereumNetworkExportTableV1 =
			xcm_builder::NetworkExportTable<EthereumBridgeTableV1>;

		pub type EthereumNetworkExportTableV2 =
			snowbridge_outbound_queue_primitives::v2::XcmFilterExporter<
				xcm_builder::NetworkExportTable<EthereumBridgeTableV2>,
				snowbridge_outbound_queue_primitives::v2::XcmForSnowbridgeV2,
			>;

		pub type EthereumAssetFromEthereum =
			IsForeignConcreteAsset<FromNetwork<UniversalLocation, EthereumNetwork>>;

		impl Contains<(Location, Junction)> for UniversalAliases {
			fn contains(alias: &(Location, Junction)) -> bool {
				UniversalAliases::get().contains(alias)
			}
		}
	}

	/// Benchmarks helper for bridging configuration.
	#[cfg(feature = "runtime-benchmarks")]
	pub struct BridgingBenchmarksHelper;

	#[cfg(feature = "runtime-benchmarks")]
	impl BridgingBenchmarksHelper {
		pub fn prepare_universal_alias() -> Option<(Location, Junction)> {
			let alias =
				to_rococo::UniversalAliases::get().into_iter().find_map(|(location, junction)| {
					match to_rococo::SiblingBridgeHubWithBridgeHubRococoInstance::get()
						.eq(&location)
					{
						true => Some((location, junction)),
						false => None,
					}
				});
			Some(alias.expect("we expect here BridgeHubWestend to Rococo mapping at least"))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_support::traits::Contains;
	use sp_runtime::traits::AccountIdConversion;

	#[test]
	fn system_chain_accumulation_accounts_allows_sibling_system_parachain() {
		let accumulation_account: [u8; 32] =
			AccumulateForwardPalletId::get().into_account_truncating();
		// A sibling system parachain with the accumulation account — use BridgeHub (1002).
		let location = Location::new(
			1,
			[Parachain(BRIDGE_HUB_ID), AccountId32 { network: None, id: accumulation_account }],
		);
		assert!(SystemChainAccumulationAccounts::contains(&location));
	}

	#[test]
	fn system_chain_accumulation_accounts_allows_relay_chain_parent() {
		let accumulation_account: [u8; 32] =
			AccumulateForwardPalletId::get().into_account_truncating();
		// Relay chain (parent) with the accumulation account.
		let location = Location::new(1, [AccountId32 { network: None, id: accumulation_account }]);
		assert!(SystemChainAccumulationAccounts::contains(&location));
	}

	#[test]
	fn system_chain_accumulation_accounts_rejects_wrong_account() {
		let wrong_account = [0u8; 32];
		// Correct sibling system parachain, but wrong account.
		let location =
			Location::new(1, [Parachain(1000), AccountId32 { network: None, id: wrong_account }]);
		assert!(!SystemChainAccumulationAccounts::contains(&location));
	}

	#[test]
	fn system_chain_accumulation_accounts_rejects_non_system_parachain() {
		let accumulation_account: [u8; 32] =
			AccumulateForwardPalletId::get().into_account_truncating();
		// Non-system parachain (id=2000, the lowest public parachain id).
		let location = Location::new(
			1,
			[Parachain(2000), AccountId32 { network: None, id: accumulation_account }],
		);
		assert!(!SystemChainAccumulationAccounts::contains(&location));
	}

	#[test]
	fn system_chain_accumulation_accounts_rejects_local_account() {
		let accumulation_account: [u8; 32] =
			AccumulateForwardPalletId::get().into_account_truncating();
		// Local account (parents=0) — not a system chain origin.
		let location = Location::new(0, [AccountId32 { network: None, id: accumulation_account }]);
		assert!(!SystemChainAccumulationAccounts::contains(&location));
	}

	#[test]
	fn system_chain_accumulation_accounts_rejects_parachain_without_account() {
		// Sibling system parachain with no account junction.
		let location = Location::new(1, [Parachain(1000)]);
		assert!(!SystemChainAccumulationAccounts::contains(&location));
	}
}
