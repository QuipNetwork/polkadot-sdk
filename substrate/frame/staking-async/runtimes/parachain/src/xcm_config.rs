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

use super::{
	AccountId, AllPalletsWithSystem, Assets, Balance, Balances, BaseDeliveryFee, CollatorSelection,
	FeeAssetId, FellowshipAdmin, ForeignAssets, ForeignAssetsInstance, GeneralAdmin, ParachainInfo,
	ParachainSystem, PolkadotXcm, PoolAssets, Runtime, RuntimeCall, RuntimeEvent, RuntimeOrigin,
	StakingAdmin, ToRococoXcmRouter, TransactionByteFee, Treasurer, TrustBackedAssetsInstance,
	Uniques, WeightToFee, XcmpQueue,
};
use assets_common::{
	matching::{FromSiblingParachain, IsForeignConcreteAsset, ParentLocation},
	TrustBackedAssetsAsLocation,
};
use frame_support::{
	parameter_types,
	traits::{
		fungible::HoldConsideration,
		tokens::imbalance::{ResolveAssetTo, ResolveTo},
		ConstU32, Contains, Equals, Everything, LinearStoragePrice, PalletInfoAccess,
	},
};
use frame_system::EnsureRoot;
use pallet_xcm::XcmPassthrough;
use parachains_common::xcm_config::{
	AllSiblingSystemParachains, AssetFeeAsExistentialDepositMultiplier, ConcreteAssetFromSystem,
	RelayOrOtherSystemParachains,
};
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_common::xcm_sender::ExponentialPrice;
use sp_runtime::traits::{AccountIdConversion, ConvertInto, TryConvertInto};
use westend_runtime_constants::{
	system_parachain::COLLECTIVES_ID, xcm::body::FELLOWSHIP_ADMIN_INDEX,
};
use xcm::latest::{prelude::*, ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH};
use xcm_builder::{
	AccountId32Aliases, AliasChildLocation, AllowExplicitUnpaidExecutionFrom,
	AllowHrmpNotificationsFromRelayChain, AllowKnownQueryResponses, AllowSubscriptionsFrom,
	AllowTopLevelPaidExecutionFrom, DenyReserveTransferToRelayChain, DenyThenTry,
	DescribeAllTerminal, DescribeFamily, EnsureXcmOrigin, FrameTransactionalProcessor,
	FungibleAdapter, FungiblesAdapter, GlobalConsensusParachainConvertsFor, HashedDescription,
	IsConcrete, LocalMint, MatchedConvertedConcreteId, NetworkExportTableItem, NoChecking,
	NonFungiblesAdapter, OriginToPluralityVoice, ParentAsSuperuser, ParentIsPreset,
	RelayChainAsNative, SendXcmFeeToAccount, SiblingParachainAsNative, SiblingParachainConvertsVia,
	SignedAccountId32AsNative, SignedToAccountId32, SingleAssetExchangeAdapter,
	SovereignPaidRemoteExporter, SovereignSignedViaLocation, StartsWith,
	StartsWithExplicitGlobalConsensus, TakeWeightCredit, TrailingSetTopicAsId, UsingComponents,
	WeightInfoBounds, WithComputedOrigin, WithLatestLocationConverter, WithUniqueTopic,
	XcmFeeManagerFromComponents,
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
	pub DapBufferAccount: AccountId = crate::staking::DapPalletId::get().into_account_truncating();
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
	// Different global consensus parachain sovereign account.
	// (Used for over-bridge transfers and reserve processing)
	GlobalConsensusParachainConvertsFor<UniversalLocation, AccountId>,
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
	// We don't track any teleports of `Balances`.
	(),
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
pub type UniquesTransactor = NonFungiblesAdapter<
	// Use this non-fungibles implementation:
	Uniques,
	// This adapter will handle any non-fungible asset from the uniques pallet.
	UniquesConvertedConcreteId,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// Does not check teleports.
	NoChecking,
	// The account to use for tracking teleports.
	CheckingAccount,
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

/// Means for transacting assets on this chain.
pub type AssetTransactors = (
	FungibleTransactor,
	FungiblesTransactor,
	ForeignFungiblesTransactor,
	PoolFungiblesTransactor,
	UniquesTransactor,
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

pub struct ParentOrParentsPlurality;
impl Contains<Location> for ParentOrParentsPlurality {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (1, []) | (1, [Plurality { .. }]))
	}
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

pub type Barrier = TrailingSetTopicAsId<
	DenyThenTry<
		DenyReserveTransferToRelayChain,
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
					// Parent, its pluralities (i.e. governance bodies) and
					// sibling parachains get free execution.
					AllowExplicitUnpaidExecutionFrom<(
						ParentOrParentsPlurality,
						RelayOrOtherSystemParachains<AllSiblingSystemParachains, Runtime>,
						FellowshipEntities,
						AmbassadorEntities,
					)>,
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

// TODO: This calls into the Assets pallet's default `BalanceToAssetBalance` implementation, which
// uses the ratio of minimum balances and requires asset sufficiency. This means that purchasing
// weight within XCM programs will still use the old way, and paying fees via asset conversion will
// only be possible when transacting locally. We should add an impl of this trait that does asset
// conversion.
pub type AssetFeeAsExistentialDepositMultiplierFeeCharger = AssetFeeAsExistentialDepositMultiplier<
	Runtime,
	WeightToFee,
	pallet_assets::BalanceToAssetBalance<Balances, Runtime, ConvertInto, TrustBackedAssetsInstance>,
	TrustBackedAssetsInstance,
>;

/// Multiplier used for dedicated `TakeFirstAssetTrader` with `ForeignAssets` instance.
pub type ForeignAssetFeeAsExistentialDepositMultiplierFeeCharger =
	AssetFeeAsExistentialDepositMultiplier<
		Runtime,
		WeightToFee,
		pallet_assets::BalanceToAssetBalance<Balances, Runtime, ConvertInto, ForeignAssetsInstance>,
		ForeignAssetsInstance,
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
);

/// Cases where a remote origin is accepted as trusted Teleporter for a given asset:
///
/// - WND with the parent Relay Chain and sibling system parachains; and
/// - Sibling parachains' assets from where they originate (as `ForeignCreators`).
pub type TrustedTeleporters = (
	ConcreteAssetFromSystem<WestendLocation>,
	IsForeignConcreteAsset<FromSiblingParachain<parachain_info::Pallet<Runtime>>>,
);

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
	type XcmEventEmitter = PolkadotXcm;
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type AssetTransactor = AssetTransactors;
	type OriginConverter = XcmOriginToTransactDispatchOrigin;
	// Asset Hub trusts only particular, pre-configured bridged locations from a different consensus
	// as reserve locations (we trust the Bridge Hub to relay the message that a reserve is being
	// held). On Westend Asset Hub, we allow Rococo Asset Hub to act as reserve for any asset native
	// to the Rococo or Ethereum ecosystems.
	type IsReserve = (
		bridging::to_rococo::RococoAssetFromAssetHubRococo,
		bridging::to_ethereum::EthereumAssetFromEthereum,
	);
	type IsTeleporter = TrustedTeleporters;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::AssetHubNextWestendXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
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
		// This trader allows to pay with `is_sufficient=true` "Foreign" assets from dedicated
		// `pallet_assets` instance - `ForeignAssets`.
		cumulus_primitives_utility::TakeFirstAssetTrader<
			AccountId,
			ForeignAssetFeeAsExistentialDepositMultiplierFeeCharger,
			ForeignAssetsConvertedConcreteId,
			ForeignAssets,
			ResolveAssetTo<StakingPot, ForeignAssets>,
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
	// We allow any origin to alias into a child sub-location (equivalent to DescendOrigin).
	type Aliasers = AliasChildLocation;
	type TransactionalProcessor = FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = PolkadotXcm;
}

parameter_types! {
	// `GeneralAdmin` pluralistic body.
	pub const GeneralAdminBodyId: BodyId = BodyId::Administration;
	// StakingAdmin pluralistic body.
	pub const StakingAdminBodyId: BodyId = BodyId::Defense;
	// FellowshipAdmin pluralistic body.
	pub const FellowshipAdminBodyId: BodyId = BodyId::Index(FELLOWSHIP_ADMIN_INDEX);
	// `Treasurer` pluralistic body.
	pub const TreasurerBodyId: BodyId = BodyId::Treasury;
}

/// Type to convert the `GeneralAdmin` origin to a Plurality `Location` value.
pub type GeneralAdminToPlurality =
	OriginToPluralityVoice<RuntimeOrigin, GeneralAdmin, GeneralAdminBodyId>;

/// Local origins on this chain are allowed to dispatch XCM sends/executions.
pub type LocalOriginToLocation =
	(GeneralAdminToPlurality, SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>);

/// Type to convert the `StakingAdmin` origin to a Plurality `Location` value.
pub type StakingAdminToPlurality =
	OriginToPluralityVoice<RuntimeOrigin, StakingAdmin, StakingAdminBodyId>;

/// Type to convert the `FellowshipAdmin` origin to a Plurality `Location` value.
pub type FellowshipAdminToPlurality =
	OriginToPluralityVoice<RuntimeOrigin, FellowshipAdmin, FellowshipAdminBodyId>;

/// Type to convert the `Treasurer` origin to a Plurality `Location` value.
pub type TreasurerToPlurality = OriginToPluralityVoice<RuntimeOrigin, Treasurer, TreasurerBodyId>;

/// Type to convert a pallet `Origin` type value into a `Location` value which represents an
/// interior location of this chain for a destination chain.
pub type LocalPalletOriginToLocation = (
	// GeneralAdmin origin to be used in XCM as a corresponding Plurality `Location` value.
	GeneralAdminToPlurality,
	// StakingAdmin origin to be used in XCM as a corresponding Plurality `Location` value.
	StakingAdminToPlurality,
	// FellowshipAdmin origin to be used in XCM as a corresponding Plurality `Location` value.
	FellowshipAdminToPlurality,
	// `Treasurer` origin to be used in XCM as a corresponding Plurality `Location` value.
	TreasurerToPlurality,
);

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
	// GlobalConsensus
	SovereignPaidRemoteExporter<
		bridging::to_ethereum::EthereumNetworkExportTable,
		XcmpQueue,
		UniversalLocation,
	>,
)>;

parameter_types! {
	pub Collectives: Location = Parachain(COLLECTIVES_ID).into_location();

	pub const DepositPerItem: Balance = crate::deposit(1, 0);
	pub const DepositPerByte: Balance = crate::deposit(0, 1);
	pub const AuthorizeAliasHoldReason: crate::RuntimeHoldReason = crate::RuntimeHoldReason::PolkadotXcm(pallet_xcm::HoldReason::AuthorizeAlias);
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
		crate::weights::xcm::AssetHubNextWestendXcmWeight<RuntimeCall>,
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
	use alloc::collections::btree_set::BTreeSet;
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

		pub BridgeTable: alloc::vec::Vec<NetworkExportTableItem> =
			alloc::vec::Vec::new().into_iter()
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
			pub BridgeTable: alloc::vec::Vec<NetworkExportTableItem> = alloc::vec![
				NetworkExportTableItem::new(
					RococoNetwork::get(),
					Some(alloc::vec![
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
				alloc::vec![
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
		use sp_std::collections::btree_set::BTreeSet;
		use testnet_parachains_constants::westend::snowbridge::{
			EthereumNetwork, INBOUND_QUEUE_PALLET_INDEX_V1,
		};

		parameter_types! {
			/// User fee for ERC20 token transfer back to Ethereum.
			/// (initially was calculated by test `OutboundQueue::calculate_fees` - ETH/WND 1/400 and fee_per_gas 20 GWEI = 2200698000000 + *25%)
			/// Needs to be more than fee calculated from DefaultFeeConfig FeeConfigRecord in snowbridge:parachain/pallets/outbound-queue/src/lib.rs
			/// Polkadot uses 10 decimals, Kusama,Rococo,Westend 12 decimals.
			pub const DefaultBridgeHubEthereumBaseFee: Balance = 2_750_872_500_000;
			pub storage BridgeHubEthereumBaseFee: Balance = DefaultBridgeHubEthereumBaseFee::get();
			pub SiblingBridgeHubWithEthereumInboundQueueInstance: Location = Location::new(
				1,
				[
					Parachain(SiblingBridgeHubParaId::get()),
					PalletInstance(INBOUND_QUEUE_PALLET_INDEX_V1)
				]
			);

			/// Set up exporters configuration.
			/// `Option<Asset>` represents static "base fee" which is used for total delivery fee calculation.
			pub BridgeTable: sp_std::vec::Vec<NetworkExportTableItem> = sp_std::vec![
				NetworkExportTableItem::new(
					EthereumNetwork::get().into(),
					Some(sp_std::vec![Junctions::Here]),
					SiblingBridgeHub::get(),
					Some((
						XcmBridgeHubRouterFeeAssetId::get(),
						BridgeHubEthereumBaseFee::get(),
					).into())
				),
			];

			/// Universal aliases
			pub UniversalAliases: BTreeSet<(Location, Junction)> = BTreeSet::from_iter(
				sp_std::vec![
					(SiblingBridgeHubWithEthereumInboundQueueInstance::get(), GlobalConsensus(EthereumNetwork::get().into())),
				]
			);

			pub EthereumBridgeTable: sp_std::vec::Vec<NetworkExportTableItem> = sp_std::vec::Vec::new().into_iter()
				.chain(BridgeTable::get())
				.collect();
		}

		pub type EthereumNetworkExportTable = xcm_builder::NetworkExportTable<EthereumBridgeTable>;

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
