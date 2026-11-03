// This file is part of Cumulus.
// SPDX-License-Identifier: Unlicense

// This is free and unencumbered software released into the public domain.

// Anyone is free to copy, modify, publish, use, compile, sell, or
// distribute this software, either in source code form or as a compiled
// binary, for any purpose, commercial or non-commercial, and by any
// means.

// In jurisdictions that recognize copyright laws, the author or authors
// of this software dedicate any and all copyright interest in the
// software to the public domain. We make this dedication for the benefit
// of the public at large and to the detriment of our heirs and
// successors. We intend this dedication to be an overt act of
// relinquishment in perpetuity of all present and future rights to this
// software under copyright law.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
// EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
// MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
// IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY CLAIM, DAMAGES OR
// OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
// ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
// OTHER DEALINGS IN THE SOFTWARE.

// For more information, please refer to <http://unlicense.org/>

//! Holds the XCM specific configuration that would otherwise be in lib.rs
//!
//! This configuration dictates how the Penpal chain will communicate with other chains.
//!
//! One of the main uses of the penpal chain will be to be a benefactor of reserve asset transfers
//! with Asset Hub as the reserve. At present no derivative tokens are minted on receipt of a
//! `ReserveAssetTransferDeposited` message but that will but the intension will be to support this
//! soon.
use super::{
	AccountId, AllPalletsWithSystem, Assets, Authorship, Balance, Balances, CollatorSelection,
	ParachainInfo, ParachainSystem, PolkadotXcm, Runtime, RuntimeCall, RuntimeEvent,
	RuntimeHoldReason, RuntimeOrigin, WeightToFee, XcmpQueue,
};
use crate::{BaseDeliveryFee, FeeAssetId, TransactionByteFee};
use core::marker::PhantomData;
use frame_support::{
	parameter_types,
	traits::{
		fungible::HoldConsideration, tokens::imbalance::ResolveAssetTo, ConstU32, Contains,
		ContainsPair, Equals, Everything, EverythingBut, Get, LinearStoragePrice, Nothing,
		PalletInfoAccess,
	},
	weights::Weight,
};
use frame_system::EnsureRoot;
use pallet_xcm::{AuthorizedAliasers, XcmPassthrough};
use parachains_common::{
	impls::NonZeroIssuance, xcm_config::ConcreteAssetFromSystem, TREASURY_PALLET_ID,
};
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_common::{impls::ToAuthor, xcm_sender::ExponentialPrice};
use sp_runtime::traits::{AccountIdConversion, Identity, TryConvertInto};
use testnet_parachains_constants::westend::currency::deposit;
use xcm::latest::{prelude::*, WESTEND_GENESIS_HASH};
use xcm_builder::{
	AccountId32Aliases, AliasChildLocation, AliasOriginRootUsingFilter,
	AllowExplicitUnpaidExecutionFrom, AllowHrmpNotificationsFromRelayChain,
	AllowKnownQueryResponses, AllowSubscriptionsFrom, AllowTopLevelPaidExecutionFrom,
	DescribeAllTerminal, DescribeFamily, DescribeTerminus, EnsureXcmOrigin,
	ExternalConsensusLocationsConverterFor, FixedWeightBounds, FrameTransactionalProcessor,
	FungibleAdapter, FungiblesAdapter, HashedDescription, IsConcrete, LocalMint, NativeAsset,
	NoChecking, ParentAsSuperuser, ParentIsPreset, RelayChainAsNative, SendXcmFeeToAccount,
	SiblingParachainAsNative, SiblingParachainConvertsVia, SignedAccountId32AsNative,
	SignedToAccountId32, SingleAssetExchangeAdapter, SovereignSignedViaLocation, StartsWith,
	TakeWeightCredit, TrailingSetTopicAsId, UsingComponents, WithComputedOrigin, WithUniqueTopic,
	XcmFeeManagerFromComponents,
};
use xcm_executor::XcmExecutor;

parameter_types! {
	pub const RelayLocation: Location = Location::parent();
	// Local native currency which is stored in `pallet_balances`
	pub const PenpalNativeCurrency: Location = Location::here();
	// The Penpal runtime is utilized for testing with various environment setups.
	// This storage item allows us to customize the `NetworkId` where Penpal is deployed.
	// By default, it is set to `Westend Network` and can be changed using `System::set_storage`.
	pub storage RelayNetworkId: NetworkId = NetworkId::ByGenesis(WESTEND_GENESIS_HASH);
	pub RelayNetwork: Option<NetworkId> = Some(RelayNetworkId::get());
	pub RelayChainOrigin: RuntimeOrigin = cumulus_pallet_xcm::Origin::Relay.into();
	pub UniversalLocation: InteriorLocation = [
		GlobalConsensus(RelayNetworkId::get()),
		Parachain(ParachainInfo::parachain_id().into())
	].into();
	pub TreasuryAccount: AccountId = TREASURY_PALLET_ID.into_account_truncating();
	pub StakingPot: AccountId = CollatorSelection::account_id();
	pub AssetsPalletIndex: u8 = <Assets as PalletInfoAccess>::index() as u8;
	pub AssetsPalletLocation: Location =
		PalletInstance(AssetsPalletIndex::get()).into();
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
	HashedDescription<AccountId, (DescribeTerminus, DescribeFamily<DescribeAllTerminal>)>,
	// Different global consensus locations sovereign accounts.
	ExternalConsensusLocationsConverterFor<UniversalLocation, AccountId>,
);

/// Means for transacting assets on this chain.
pub type FungibleTransactor = FungibleAdapter<
	// Use this currency:
	Balances,
	// Use this currency when it is a fungible asset matching the given location or name:
	IsConcrete<PenpalNativeCurrency>,
	// Do a simple punn to convert an AccountId32 Location into a native chain account ID:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We don't track any teleports.
	(),
>;

/// Means for transacting assets besides the native currency on this chain that start with a local
/// location pattern.
pub type LocalFungiblesTransactor = FungiblesAdapter<
	// Use this fungibles implementation:
	Assets,
	// Only allow locations that are local.
	LocalAssetsConvertedConcreteId,
	// Convert an XCM Location into a local account id:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We only want to allow teleports of known assets. We use non-zero issuance as an indication
	// that this asset is known.
	LocalMint<NonZeroIssuance<AccountId, Assets>>,
	// The account to use for tracking teleports.
	CheckingAccount,
>;

// Using the latest `Location`, we don't need to worry about migrations for Penpal.
pub type AssetsAssetId = Location;
pub type ForeignAssetsConvertedConcreteId = xcm_builder::MatchedConvertedConcreteId<
	Location,
	Balance,
	EverythingBut<(
		// Here we rely on fact that something like this works:
		// assert!(Location::new(1,
		// [Parachain(100)]).starts_with(&Location::parent()));
		// assert!([Parachain(100)].into().starts_with(&Here));
		StartsWith<assets_common::matching::LocalLocationPattern>,
	)>,
	Identity,
	TryConvertInto,
>;

pub type LocalAssetsConvertedConcreteId = xcm_builder::MatchedConvertedConcreteId<
	Location,
	Balance,
	// Only allow patterns with local
	(
		// Here we rely on fact that something like this works:
		// assert!(Location::new(1,
		// [Parachain(100)]).starts_with(&Location::parent()));
		// assert!([Parachain(100)].into().starts_with(&Here));
		StartsWith<assets_common::matching::LocalLocationPattern>,
	),
	Identity,
	TryConvertInto,
>;

/// Means for transacting foreign assets from different global consensus.
pub type ForeignFungiblesTransactor = FungiblesAdapter<
	// Use this fungibles implementation:
	Assets,
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

/// Means for transacting assets on this chain.
pub type AssetTransactors =
	(FungibleTransactor, ForeignFungiblesTransactor, LocalFungiblesTransactor);

/// This is the type we use to convert an (incoming) XCM origin into a local `Origin` instance,
/// ready for dispatching a transaction with Xcm's `Transact`. There is an `OriginKind` which can
/// biases the kind of local `Origin` it will become.
pub type XcmOriginToTransactDispatchOrigin = (
	// Sovereign account converter; this attempts to derive an `AccountId` from the origin location
	// using `LocationToAccountId` and then turn that into the usual `Signed` origin. Useful for
	// foreign chains who want to have a local sovereign account on this chain which they control.
	SovereignSignedViaLocation<LocationToAccountId, RuntimeOrigin>,
	// Native converter for Relay-chain (Parent) location; will convert to a `Relay` origin when
	// recognized.
	RelayChainAsNative<RelayChainOrigin, RuntimeOrigin>,
	// Native converter for sibling Parachains; will convert to a `SiblingPara` origin when
	// recognized.
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
	pub const RootLocation: Location = Location::here();
	// One XCM operation is 1_000_000_000 weight - almost certainly a conservative estimate.
	pub UnitWeightCost: Weight = Weight::from_parts(1_000_000_000, 64 * 1024);
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub XcmAssetFeesReceiver: Option<AccountId> = Authorship::author();
}

pub struct ParentOrParentsExecutivePlurality;
impl Contains<Location> for ParentOrParentsExecutivePlurality {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (1, []) | (1, [Plurality { id: BodyId::Executive, .. }]))
	}
}

pub type Barrier = TrailingSetTopicAsId<(
	TakeWeightCredit,
	// Expected responses are OK.
	AllowKnownQueryResponses<PolkadotXcm>,
	// Allow XCMs with some computed origins to pass through.
	WithComputedOrigin<
		(
			// If the message is one that immediately attempts to pay for execution, then
			// allow it.
			AllowTopLevelPaidExecutionFrom<Everything>,
			// Parent and its pluralities (i.e. governance bodies) get free execution.
			AllowExplicitUnpaidExecutionFrom<
				(ParentOrParentsExecutivePlurality,),
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
)>;

/// Type alias to conveniently refer to `frame_system`'s `Config::AccountId`.
pub type AccountIdOf<R> = <R as frame_system::Config>::AccountId;

/// Asset filter that allows all assets from a certain location matching asset id.
pub struct AssetPrefixFrom<Prefix, Origin>(PhantomData<(Prefix, Origin)>);
impl<Prefix, Origin> ContainsPair<Asset, Location> for AssetPrefixFrom<Prefix, Origin>
where
	Prefix: Get<Location>,
	Origin: Get<Location>,
{
	fn contains(asset: &Asset, origin: &Location) -> bool {
		let loc = Origin::get();
		&loc == origin &&
			matches!(asset, Asset { id: AssetId(asset_loc), fun: Fungible(_a) }
			if asset_loc.starts_with(&Prefix::get()))
	}
}

type AssetsFrom<T> = AssetPrefixFrom<T, T>;

// This asset can be added to AH as Asset and reserved transfer between Penpal and AH
pub const RESERVABLE_ASSET_ID: u32 = 1;
// This asset can be added to AH as ForeignAsset and teleported between Penpal and AH
pub const PEN2_TELEPORTABLE_GENERAL_INDEX: u32 = 2;

pub const ASSET_HUB_ASSETS_PALLET_ID: u8 = 50;
pub const ASSET_HUB_ID: u32 = 1000;

pub const PENPAL_ASSETS_PALLET_ID: u8 = 50;

pub const USDT_ASSET_ID: u128 = 1984;

pub const SEPOLIA_ID: u64 = 11155111;
// The minimum balance for assets pre-registered in emulated tests.
pub const ETHER_MIN_BALANCE: u128 = 1000;
pub const USDT_ED: Balance = 70_000;

parameter_types! {
	/// The location that this chain recognizes as the Relay network's Asset Hub.
	pub SystemAssetHubLocation: Location = Location::new(1, [Parachain(ASSET_HUB_ID)]);
	// the Relay Chain's Asset Hub's Assets pallet index
	pub SystemAssetHubAssetsPalletLocation: Location =
		Location::new(1, [Parachain(ASSET_HUB_ID), PalletInstance(ASSET_HUB_ASSETS_PALLET_ID)]);
	pub CheckingAccount: AccountId = PolkadotXcm::check_account();
	pub LocalReservableFromAssetHub: Location = Location::new(
		1,
		[Parachain(ASSET_HUB_ID), PalletInstance(ASSET_HUB_ASSETS_PALLET_ID), GeneralIndex(RESERVABLE_ASSET_ID.into())]
	);
	pub UsdtFromAssetHub: Location = Location::new(
		1,
		[Parachain(ASSET_HUB_ID), PalletInstance(ASSET_HUB_ASSETS_PALLET_ID), GeneralIndex(USDT_ASSET_ID)],
	);

	pub EthFromEthereum: Location = Location::new(
		2,
		[GlobalConsensus(Ethereum { chain_id: SEPOLIA_ID})],
	);

	/// A PEN2 test asset.
	pub LocalPen2Asset: Location = Location::new(0, [PalletInstance(PENPAL_ASSETS_PALLET_ID), GeneralIndex(PEN2_TELEPORTABLE_GENERAL_INDEX.into())]);

	/// The Penpal runtime is utilized for testing with various environment setups.
	/// This storage item provides the opportunity to customize testing scenarios
	/// by configuring the trusted asset from the `SystemAssetHub`.
	///
	/// By default, it is configured as a `SystemAssetHubLocation` and can be modified using `System::set_storage`.
	pub storage CustomizableAssetFromSystemAssetHub: Location = SystemAssetHubLocation::get();
	pub storage LocalTeleportableToAssetHub: Location = LocalPen2Asset::get();

	pub const NativeAssetId: AssetId = AssetId(Location::here());
	pub const NativeAssetFilter: AssetFilter = Wild(AllOf { fun: WildFungible, id: NativeAssetId::get() });
	pub AssetHubTrustedTeleporter: (AssetFilter, Location) = (NativeAssetFilter::get(), SystemAssetHubLocation::get());
}

/// Accepts asset with ID `AssetLocation` and is coming from `Origin` chain.
pub struct AssetFromChain<AssetLocation, Origin>(PhantomData<(AssetLocation, Origin)>);
impl<AssetLocation: Get<Location>, Origin: Get<Location>> ContainsPair<Asset, Location>
	for AssetFromChain<AssetLocation, Origin>
{
	fn contains(asset: &Asset, origin: &Location) -> bool {
		tracing::trace!(target: "xcm::contains", ?asset, ?origin, "AssetFromChain");
		*origin == Origin::get() &&
			matches!(asset.id.clone(), AssetId(id) if id == AssetLocation::get())
	}
}

pub type TrustedReserves = (
	NativeAsset,
	ConcreteAssetFromSystem<RelayLocation>,
	AssetsFrom<SystemAssetHubLocation>,
	AssetPrefixFrom<CustomizableAssetFromSystemAssetHub, SystemAssetHubLocation>,
);

pub type TrustedTeleporters = (
	AssetFromChain<PenpalNativeCurrency, SystemAssetHubLocation>,
	AssetFromChain<LocalTeleportableToAssetHub, SystemAssetHubLocation>,
	// This is used in the `IsTeleporter` configuration, meaning it accepts
	// native tokens teleported from Asset Hub.
	xcm_builder::Case<AssetHubTrustedTeleporter>,
);

/// Aliasing rules that are pure computation and thus cheap enough to also be evaluated by
/// barriers, before any payment is taken.
///
/// - Allow any origin to alias into a child sub-location (equivalent to DescendOrigin),
/// - Allow AssetHub root to alias into anything.
///
/// `AuthorizedAliasers` is deliberately NOT part of this: it reads storage, which is more than a
/// barrier is allowed to cost.
pub type CheapTrustedAliasers =
	(AliasChildLocation, AliasOriginRootUsingFilter<SystemAssetHubLocation, Everything>);

/// Defines origin aliasing rules for this chain. Used by the executor.
///
/// - Allow all the cheap aliasing rules also used by the barriers,
/// - Allow origins explicitly authorized by the alias target location.
pub type TrustedAliasers = (CheapTrustedAliasers, AuthorizedAliasers<Runtime>);

pub type WaivedLocations = Equals<RootLocation>;

/// Asset converter for pool assets.
/// Used to convert assets in pools to the asset required for fee payment.
/// The pool must be between the first asset and the one required for fee payment.
/// This type allows paying fees with any asset in a pool with the asset required for fee payment.
pub type PoolAssetsExchanger = SingleAssetExchangeAdapter<
	crate::AssetConversion,
	crate::NativeAndAssets,
	(LocalAssetsConvertedConcreteId, ForeignAssetsConvertedConcreteId),
	AccountId,
>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = PolkadotXcm;
	// How to withdraw and deposit an asset.
	type AssetTransactor = AssetTransactors;
	type OriginConverter = XcmOriginToTransactDispatchOrigin;
	type IsReserve = TrustedReserves;
	// no teleport trust established with other chains
	type IsTeleporter = TrustedTeleporters;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = FixedWeightBounds<UnitWeightCost, RuntimeCall, MaxInstructions>;
	type Trader = (
		// Allow native asset to pay the execution fee
		UsingComponents<WeightToFee, PenpalNativeCurrency, AccountId, Balances, ToAuthor<Runtime>>,
		// This trader allows to pay with any assets exchangeable to native asset with
		// [`AssetConversion`].
		cumulus_primitives_utility::SwapFirstAssetTrader<
			PenpalNativeCurrency,
			crate::AssetConversion,
			WeightToFee,
			crate::NativeAndAssets,
			(LocalAssetsConvertedConcreteId, ForeignAssetsConvertedConcreteId),
			ResolveAssetTo<StakingPot, crate::NativeAndAssets>,
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

parameter_types! {
	pub const DepositPerItem: Balance = deposit(1, 0);
	pub const DepositPerByte: Balance = deposit(0, 1);
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
	type Weigher = FixedWeightBounds<UnitWeightCost, RuntimeCall, MaxInstructions>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;

	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	// ^ Override for AdvertisedXcmVersion default
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = ();
	type TrustedLockers = ();
	type SovereignAccountOf = LocationToAccountId;
	type MaxLockers = ConstU32<8>;
	type WeightInfo = pallet_xcm::TestWeightInfo;
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
