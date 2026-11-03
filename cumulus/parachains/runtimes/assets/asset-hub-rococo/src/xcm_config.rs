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
	FeeAssetId, ForeignAssets, ParachainInfo, ParachainSystem, PolkadotXcm, PoolAssets, Runtime,
	RuntimeCall, RuntimeEvent, RuntimeHoldReason, RuntimeOrigin, ToWestendXcmRouter,
	TransactionByteFee, Uniques, WeightToFee, XcmpQueue,
};
use assets_common::{
	matching::{
		FromNetwork, IsForeignConcreteAsset, NonTeleportableAssetFromTrustedReserve,
		ParentLocation, TeleportableAssetWithTrustedReserve,
	},
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
use pallet_xcm::{AuthorizedAliasers, XcmPassthrough};
use parachains_common::{
	xcm_config::{
		AllSiblingSystemParachains, ConcreteAssetFromSystem, ParentRelayOrSiblingParachains,
		RelayOrOtherSystemParachains,
	},
	TREASURY_PALLET_ID,
};
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_common::xcm_sender::ExponentialPrice;
use rococo_runtime_constants::system_parachain::ASSET_HUB_ID;
use sp_runtime::traits::{AccountIdConversion, TryConvertInto};
use testnet_parachains_constants::rococo::snowbridge::{
	EthereumNetwork, INBOUND_QUEUE_PALLET_INDEX,
};
use xcm::latest::{prelude::*, ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH};
use xcm_builder::{
	unique_instances::UniqueInstancesAdapter, AccountId32Aliases, AliasChildLocation,
	AllowExplicitUnpaidExecutionFrom, AllowHrmpNotificationsFromRelayChain,
	AllowKnownQueryResponses, AllowSubscriptionsFrom, AllowTopLevelPaidExecutionFrom,
	DenyRecursively, DenyReserveTransferToRelayChain, DenyThenTry, DescribeAllTerminal,
	DescribeFamily, EnsureXcmOrigin, ExternalConsensusLocationsConverterFor,
	FrameTransactionalProcessor, FungibleAdapter, FungiblesAdapter, HashedDescription, IsConcrete,
	LocalMint, MatchInClassInstances, MatchedConvertedConcreteId, NetworkExportTableItem,
	NoChecking, ParentAsSuperuser, ParentIsPreset, RelayChainAsNative, SendXcmFeeToAccount,
	SiblingParachainAsNative, SiblingParachainConvertsVia, SignedAccountId32AsNative,
	SignedToAccountId32, SingleAssetExchangeAdapter, SovereignPaidRemoteExporter,
	SovereignSignedViaLocation, StartsWith, StartsWithExplicitGlobalConsensus, TakeWeightCredit,
	TrailingSetTopicAsId, UsingComponents, WeightInfoBounds, WithComputedOrigin,
	WithLatestLocationConverter, WithUniqueTopic, XcmFeeManagerFromComponents,
};
use xcm_executor::XcmExecutor;

parameter_types! {
	pub const RootLocation: Location = Location::here();
	pub const TokenLocation: Location = Location::parent();
	pub const RelayNetwork: NetworkId = NetworkId::ByGenesis(ROCOCO_GENESIS_HASH);
	pub const AssetHubParaId: crate::ParaId = crate::ParaId::new(ASSET_HUB_ID);
	pub RelayChainOrigin: RuntimeOrigin = cumulus_pallet_xcm::Origin::Relay.into();
	pub UniversalLocation: InteriorLocation =
		[GlobalConsensus(RelayNetwork::get()), Parachain(ParachainInfo::parachain_id().into())].into();
	pub UniversalLocationNetworkId: NetworkId = UniversalLocation::get().global_consensus().unwrap();
	pub TrustBackedAssetsPalletLocation: Location =
		PalletInstance(TrustBackedAssetsPalletIndex::get()).into();
	pub TrustBackedAssetsPalletIndex: u8 = <Assets as PalletInfoAccess>::index() as u8;
	pub TrustBackedAssetsPalletLocationV3: xcm::v3::Location =
		xcm::v3::Junction::PalletInstance(<Assets as PalletInfoAccess>::index() as u8).into();
	pub PoolAssetsPalletLocationV3: xcm::v3::Location =
		xcm::v3::Junction::PalletInstance(<PoolAssets as PalletInfoAccess>::index() as u8).into();
	pub ForeignAssetsPalletLocation: Location =
		PalletInstance(<ForeignAssets as PalletInfoAccess>::index() as u8).into();
	pub PoolAssetsPalletLocation: Location =
		PalletInstance(<PoolAssets as PalletInfoAccess>::index() as u8).into();
	pub UniquesPalletLocation: Location =
		PalletInstance(<Uniques as PalletInfoAccess>::index() as u8).into();
	pub CheckingAccount: AccountId = PolkadotXcm::check_account();
	pub const GovernanceLocation: Location = Location::parent();
	pub StakingPot: AccountId = CollatorSelection::account_id();
	pub TreasuryAccount: AccountId = TREASURY_PALLET_ID.into_account_truncating();
	pub RelayTreasuryLocation: Location = (Parent, PalletInstance(rococo_runtime_constants::TREASURY_PALLET_ID)).into();
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
	IsConcrete<TokenLocation>,
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
		// Ignore assets that start explicitly with our `GlobalConsensus(NetworkId)`, means:
		// - foreign assets from our consensus should be: `Location {parents: 1, X*(Parachain(xyz),
		//   ..)}`
		// - foreign assets outside our consensus with the same `GlobalConsensus(NetworkId)` won't
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
					// Parent, its pluralities (i.e. governance bodies), relay treasury pallet and
					// BridgeHub get free execution.
					AllowExplicitUnpaidExecutionFrom<
						(
							ParentOrParentsPlurality,
							Equals<RelayTreasuryLocation>,
							Equals<bridging::SiblingBridgeHub>,
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
	Equals<RootLocation>,
	RelayOrOtherSystemParachains<AllSiblingSystemParachains, Runtime>,
	Equals<RelayTreasuryLocation>,
);

// Asset Hub trusts only particular, pre-configured bridged locations from a different consensus
// as reserve locations (we trust the Bridge Hub to relay the message that a reserve is being
// held). On Rococo Asset Hub, we allow Westend Asset Hub to act as reserve for any asset native
// to the Westend ecosystem. We also allow Ethereum contracts to act as reserves for the foreign
// assets identified by the same respective contracts locations.
pub type TrustedReserves = (
	bridging::to_westend::WestendOrEthereumAssetFromAssetHubWestend,
	bridging::to_ethereum::EthereumAssetFromEthereum,
	IsForeignConcreteAsset<
		NonTeleportableAssetFromTrustedReserve<AssetHubParaId, crate::ForeignAssets>,
	>,
);

/// Cases where a remote origin is accepted as trusted Teleporter for a given asset:
///
/// - ROC with the parent Relay Chain and sibling system parachains; and
/// - Sibling parachains' assets from where they originate (as `ForeignCreators`).
pub type TrustedTeleporters = (
	ConcreteAssetFromSystem<TokenLocation>,
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
		crate::weights::xcm::AssetHubRococoXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	type Trader = (
		UsingComponents<
			WeightToFee,
			TokenLocation,
			AccountId,
			Balances,
			ResolveTo<StakingPot, Balances>,
		>,
		cumulus_primitives_utility::SwapFirstAssetTrader<
			TokenLocation,
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
		SendXcmFeeToAccount<Self::AssetTransactor, TreasuryAccount>,
	>;
	type MessageExporter = ();
	type UniversalAliases =
		(bridging::to_westend::UniversalAliases, bridging::to_ethereum::UniversalAliases);
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	// We allow any origin to alias into a child sub-location (equivalent to DescendOrigin).
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
	// Router which wraps and sends xcm to BridgeHub to be delivered to the Westend
	// GlobalConsensus
	ToWestendXcmRouter,
	// Router which wraps and sends xcm to BridgeHub to be delivered to the Ethereum
	// GlobalConsensus
	SovereignPaidRemoteExporter<bridging::EthereumNetworkExportTable, XcmpQueue, UniversalLocation>,
)>;

parameter_types! {
	pub const DepositPerItem: Balance = crate::deposit(1, 0);
	pub const DepositPerByte: Balance = crate::deposit(0, 1);
	pub const AuthorizeAliasHoldReason: RuntimeHoldReason = RuntimeHoldReason::PolkadotXcm(pallet_xcm::HoldReason::AuthorizeAlias);
}

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	// We want to disallow users sending (arbitrary) XCMs from this chain.
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, ()>;
	type XcmRouter = XcmRouter;
	// We support local origins dispatching XCM executions.
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Everything;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::AssetHubRococoXcmWeight<RuntimeCall>,
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
	use alloc::collections::btree_set::BTreeSet;
	use assets_common::matching;

	// common/shared parameters
	parameter_types! {
		/// Base price of every byte of the Rococo -> Westend message. Can be adjusted via
		/// governance `set_storage` call.
		///
		/// Default value is our estimation of the:
		///
		/// 1) an approximate cost of XCM execution (`ExportMessage` and surroundings) at Rococo bridge hub;
		///
		/// 2) the approximate cost of Rococo -> Westend message delivery transaction on Westend Bridge Hub,
		///    converted into ROCs using 1:1 conversion rate;
		///
		/// 3) the approximate cost of Rococo -> Westend message confirmation transaction on Rococo Bridge Hub.
		pub storage XcmBridgeHubRouterBaseFee: Balance =
			bp_bridge_hub_rococo::BridgeHubRococoBaseXcmFeeInRocs::get()
				.saturating_add(bp_bridge_hub_westend::BridgeHubWestendBaseDeliveryFeeInWnds::get())
				.saturating_add(bp_bridge_hub_rococo::BridgeHubRococoBaseConfirmationFeeInRocs::get());
		/// Price of every byte of the Rococo -> Westend message. Can be adjusted via
		/// governance `set_storage` call.
		pub storage XcmBridgeHubRouterByteFee: Balance = TransactionByteFee::get();

		pub SiblingBridgeHubParaId: u32 = bp_bridge_hub_rococo::BRIDGE_HUB_ROCOCO_PARACHAIN_ID;
		pub SiblingBridgeHub: Location = Location::new(1, [Parachain(SiblingBridgeHubParaId::get())]);
		/// Router expects payment with this `AssetId`.
		/// (`AssetId` has to be aligned with `BridgeTable`)
		pub XcmBridgeHubRouterFeeAssetId: AssetId = TokenLocation::get().into();

		pub BridgeTable: alloc::vec::Vec<NetworkExportTableItem> =
			alloc::vec::Vec::new().into_iter()
			.chain(to_westend::BridgeTable::get())
			.collect();

		pub EthereumBridgeTable: alloc::vec::Vec<NetworkExportTableItem> =
			alloc::vec::Vec::new().into_iter()
			.chain(to_ethereum::BridgeTable::get())
			.collect();
	}

	pub type NetworkExportTable = xcm_builder::NetworkExportTable<BridgeTable>;

	pub type EthereumNetworkExportTable = xcm_builder::NetworkExportTable<EthereumBridgeTable>;

	pub mod to_westend {
		use super::*;

		parameter_types! {
			pub SiblingBridgeHubWithBridgeHubWestendInstance: Location = Location::new(
				1,
				[
					Parachain(SiblingBridgeHubParaId::get()),
					PalletInstance(bp_bridge_hub_rococo::WITH_BRIDGE_ROCOCO_TO_WESTEND_MESSAGES_PALLET_INDEX)
				]
			);

			pub const WestendNetwork: NetworkId = NetworkId::ByGenesis(WESTEND_GENESIS_HASH);
			pub const EthereumNetwork: NetworkId = NetworkId::Ethereum { chain_id: 11155111 };
			pub WestendEcosystem: Location = Location::new(2, [GlobalConsensus(WestendNetwork::get())]);
			pub EthereumEcosystem: Location = Location::new(2, [GlobalConsensus(EthereumNetwork::get())]);
			pub WndLocation: Location = Location::new(2, [GlobalConsensus(WestendNetwork::get())]);
			pub AssetHubWestend: Location = Location::new(2, [
				GlobalConsensus(WestendNetwork::get()),
				Parachain(bp_asset_hub_westend::ASSET_HUB_WESTEND_PARACHAIN_ID)
			]);

			/// Set up exporters configuration.
			/// `Option<Asset>` represents static "base fee" which is used for total delivery fee calculation.
			pub BridgeTable: alloc::vec::Vec<NetworkExportTableItem> = alloc::vec![
				NetworkExportTableItem::new(
					WestendNetwork::get(),
					Some(alloc::vec![
						AssetHubWestend::get().interior.split_global().expect("invalid configuration for AssetHubWestend").1,
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
					(SiblingBridgeHubWithBridgeHubWestendInstance::get(), GlobalConsensus(WestendNetwork::get()))
				]
			);
		}

		impl Contains<(Location, Junction)> for UniversalAliases {
			fn contains(alias: &(Location, Junction)) -> bool {
				UniversalAliases::get().contains(alias)
			}
		}

		/// Allow any asset native to the Westend or Ethereum ecosystems if it comes from Westend
		/// Asset Hub.
		pub type WestendOrEthereumAssetFromAssetHubWestend = matching::RemoteAssetFromLocation<
			(StartsWith<WestendEcosystem>, StartsWith<EthereumEcosystem>),
			AssetHubWestend,
		>;
	}

	pub mod to_ethereum {
		use super::*;

		parameter_types! {
			/// User fee for ERC20 token transfer back to Ethereum.
			/// (initially was calculated by test `OutboundQueue::calculate_fees` - ETH/ROC 1/400 and fee_per_gas 20 GWEI = 2200698000000 + *25%)
			/// Needs to be more than fee calculated from DefaultFeeConfig FeeConfigRecord in snowbridge:parachain/pallets/outbound-queue/src/lib.rs
			/// Polkadot uses 10 decimals, Kusama and Rococo 12 decimals.
			pub const DefaultBridgeHubEthereumBaseFee: Balance = 3_833_568_200_000;
			pub storage BridgeHubEthereumBaseFee: Balance = DefaultBridgeHubEthereumBaseFee::get();
			pub SiblingBridgeHubWithEthereumInboundQueueInstance: Location = Location::new(
				1,
				[
					Parachain(SiblingBridgeHubParaId::get()),
					PalletInstance(INBOUND_QUEUE_PALLET_INDEX)
				]
			);

			/// Set up exporters configuration.
			/// `Option<Asset>` represents static "base fee" which is used for total delivery fee calculation.
			pub BridgeTable: alloc::vec::Vec<NetworkExportTableItem> = alloc::vec![
				NetworkExportTableItem::new(
					EthereumNetwork::get().into(),
					Some(alloc::vec![Junctions::Here]),
					SiblingBridgeHub::get(),
					Some((
						XcmBridgeHubRouterFeeAssetId::get(),
						BridgeHubEthereumBaseFee::get(),
					).into())
				),
			];

			/// Universal aliases
			pub UniversalAliases: BTreeSet<(Location, Junction)> = BTreeSet::from_iter(
				alloc::vec![
					(SiblingBridgeHubWithEthereumInboundQueueInstance::get(), GlobalConsensus(EthereumNetwork::get().into())),
				]
			);
		}

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
				to_westend::UniversalAliases::get()
					.into_iter()
					.find_map(|(location, junction)| {
						match to_westend::SiblingBridgeHubWithBridgeHubWestendInstance::get()
							.eq(&location)
						{
							true => Some((location, junction)),
							false => None,
						}
					});
			Some(alias.expect("we expect here BridgeHubRococo to Westend mapping at least"))
		}
	}
}
