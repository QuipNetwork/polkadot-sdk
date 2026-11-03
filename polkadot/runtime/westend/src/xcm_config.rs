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

//! XCM configurations for Westend.

use super::{
	parachains_origin, AccountId, AllPalletsWithSystem, Balances, Dmp, ParaId, Runtime,
	RuntimeCall, RuntimeEvent, RuntimeOrigin, TransactionByteFee, WeightToFee, XcmPallet,
};
use frame_support::{
	parameter_types,
	traits::{Contains, Disabled, Equals, Everything, Nothing},
};
use frame_system::EnsureRoot;
use pallet_xcm::XcmPassthrough;
use polkadot_runtime_common::{
	xcm_sender::{ChildParachainRouter, ExponentialPrice},
	ToAuthor,
};
use sp_core::ConstU32;
use westend_runtime_constants::{currency::CENTS, system_parachain::*};
use xcm::latest::{prelude::*, WESTEND_GENESIS_HASH};
use xcm_builder::{
	AccountId32Aliases, AliasChildLocation, AllowExplicitUnpaidExecutionFrom,
	AllowKnownQueryResponses, AllowSubscriptionsFrom, AllowTopLevelPaidExecutionFrom,
	ChildParachainAsNative, ChildParachainConvertsVia, DescribeAllTerminal, DescribeFamily,
	FrameTransactionalProcessor, FungibleAdapter, HashedDescription, IsChildSystemParachain,
	IsConcrete, LocationAsSuperuser, MintLocation, SendXcmFeeToAccount, SignedAccountId32AsNative,
	SignedToAccountId32, SovereignSignedViaLocation, TakeWeightCredit, TrailingSetTopicAsId,
	UsingComponents, WeightInfoBounds, WithComputedOrigin, WithUniqueTopic,
	XcmFeeManagerFromComponents,
};
use xcm_executor::XcmExecutor;

parameter_types! {
	pub const TokenLocation: Location = Here.into_location();
	pub const RootLocation: Location = Location::here();
	pub const ThisNetwork: NetworkId = ByGenesis(WESTEND_GENESIS_HASH);
	pub UniversalLocation: InteriorLocation = [GlobalConsensus(ThisNetwork::get())].into();
	pub CheckAccount: AccountId = XcmPallet::check_account();
	/// Westend does not have mint authority anymore after the Asset Hub migration.
	pub TeleportTracking: Option<(AccountId, MintLocation)> = None;
	pub AccumulateAccount: AccountId = pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	/// The asset ID for the asset that we use to pay for message delivery fees.
	pub FeeAssetId: AssetId = AssetId(TokenLocation::get());
	/// The base fee for the message delivery fees.
	pub const BaseDeliveryFee: u128 = CENTS.saturating_mul(3);
	// Fellows pluralistic body.
	pub const FellowsBodyId: BodyId = BodyId::Technical;
	/// The accumulation account location on this chain.
	pub AccumulateForwardLocation: Location = {
		AccountId32 { network: None, id: AccumulateAccount::get().into() }.into()
	};
}

pub type LocationConverter = (
	// We can convert a child parachain using the standard `AccountId` conversion.
	ChildParachainConvertsVia<ParaId, AccountId>,
	// We can directly alias an `AccountId32` into a local account.
	AccountId32Aliases<ThisNetwork, AccountId>,
	// Foreign locations alias into accounts according to a hash of their standard description.
	HashedDescription<AccountId, DescribeFamily<DescribeAllTerminal>>,
);

pub type LocalAssetTransactor = FungibleAdapter<
	// Use this currency:
	Balances,
	// Use this currency when it is a fungible asset matching the given location or name:
	IsConcrete<TokenLocation>,
	// We can convert the Locations with our converter above:
	LocationConverter,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	TeleportTracking,
>;

type LocalOriginConverter = (
	// Asset Hub can gain root on the relay chain.
	LocationAsSuperuser<Equals<AssetHub>, RuntimeOrigin>,
	// If the origin kind is `Sovereign`, then return a `Signed` origin with the account determined
	// by the `LocationConverter` converter.
	SovereignSignedViaLocation<LocationConverter, RuntimeOrigin>,
	// If the origin kind is `Native` and the XCM origin is a child parachain, then we can express
	// it with the special `parachains_origin::Origin` origin variant.
	ChildParachainAsNative<parachains_origin::Origin, RuntimeOrigin>,
	// If the origin kind is `Native` and the XCM origin is the `AccountId32` location, then it can
	// be expressed using the `Signed` origin variant.
	SignedAccountId32AsNative<ThisNetwork, RuntimeOrigin>,
	// Xcm origins can be represented natively under the Xcm pallet's Xcm origin.
	XcmPassthrough<RuntimeOrigin>,
);

pub type PriceForChildParachainDelivery =
	ExponentialPrice<FeeAssetId, BaseDeliveryFee, TransactionByteFee, Dmp>;

/// The XCM router. When we want to send an XCM message, we use this type. It amalgamates all of our
/// individual routers.
pub type XcmRouter = WithUniqueTopic<
	// Only one router so far - use DMP to communicate with child parachains.
	ChildParachainRouter<Runtime, XcmPallet, PriceForChildParachainDelivery>,
>;

parameter_types! {
	pub AssetHub: Location = Parachain(ASSET_HUB_ID).into_location();
	pub AssetHubNext: Location = Parachain(ASSET_HUB_NEXT_ID).into_location();
	pub Collectives: Location = Parachain(COLLECTIVES_ID).into_location();
	pub BridgeHub: Location = Parachain(BRIDGE_HUB_ID).into_location();
	pub Encointer: Location = Parachain(ENCOINTER_ID).into_location();
	pub People: Location = Parachain(PEOPLE_ID).into_location();
	pub Broker: Location = Parachain(BROKER_ID).into_location();
	pub Wnd: AssetFilter = Wild(AllOf { fun: WildFungible, id: AssetId(TokenLocation::get()) });
	pub WndForAssetHub: (AssetFilter, Location) = (Wnd::get(), AssetHub::get());
	pub WndForAssetHubNext: (AssetFilter, Location) = (Wnd::get(), AssetHubNext::get());
	pub WndForCollectives: (AssetFilter, Location) = (Wnd::get(), Collectives::get());
	pub WndForBridgeHub: (AssetFilter, Location) = (Wnd::get(), BridgeHub::get());
	pub WndForEncointer: (AssetFilter, Location) = (Wnd::get(), Encointer::get());
	pub WndForPeople: (AssetFilter, Location) = (Wnd::get(), People::get());
	pub WndForBroker: (AssetFilter, Location) = (Wnd::get(), Broker::get());
	pub MaxInstructions: u32 = 100;
	pub MaxAssetsIntoHolding: u32 = 64;
}

pub type TrustedTeleporters = (
	xcm_builder::Case<WndForAssetHub>,
	xcm_builder::Case<WndForAssetHubNext>,
	xcm_builder::Case<WndForCollectives>,
	xcm_builder::Case<WndForBridgeHub>,
	xcm_builder::Case<WndForEncointer>,
	xcm_builder::Case<WndForPeople>,
	xcm_builder::Case<WndForBroker>,
);

pub struct OnlyParachains;
impl Contains<Location> for OnlyParachains {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (0, [Parachain(_)]))
	}
}

pub struct Fellows;
impl Contains<Location> for Fellows {
	fn contains(location: &Location) -> bool {
		matches!(
			location.unpack(),
			(0, [Parachain(COLLECTIVES_ID), Plurality { id: BodyId::Technical, .. }])
		)
	}
}

pub struct LocalPlurality;
impl Contains<Location> for LocalPlurality {
	fn contains(loc: &Location) -> bool {
		matches!(loc.unpack(), (0, [Plurality { .. }]))
	}
}

/// The barriers one of which must be passed for an XCM message to be executed.
pub type Barrier = TrailingSetTopicAsId<(
	// Weight that is paid for may be consumed.
	TakeWeightCredit,
	// Expected responses are OK.
	AllowKnownQueryResponses<XcmPallet>,
	WithComputedOrigin<
		(
			// If the message is one that immediately attempts to pay for execution, then allow it.
			AllowTopLevelPaidExecutionFrom<Everything>,
			// Subscriptions for version tracking are OK.
			AllowSubscriptionsFrom<OnlyParachains>,
			// Messages from system parachains or the Fellows plurality need not pay for execution.
			AllowExplicitUnpaidExecutionFrom<(IsChildSystemParachain<ParaId>, Fellows)>,
		),
		UniversalLocation,
		ConstU32<8>,
	>,
)>;

/// Locations that will not be charged fees in the executor, neither for execution nor delivery.
/// We only waive fees for system functions, which these locations represent.
pub type WaivedLocations =
	(SystemParachains, Equals<RootLocation>, LocalPlurality, Equals<AccumulateForwardLocation>);

/// We let locations alias into child locations of their own.
/// This is a very simple aliasing rule, mimicking the behaviour of
/// the `DescendOrigin` instruction.
pub type Aliasers = AliasChildLocation;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = XcmPallet;
	type AssetTransactor = LocalAssetTransactor;
	type OriginConverter = LocalOriginConverter;
	type IsReserve = ();
	type IsTeleporter = TrustedTeleporters;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::WestendXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	// TODO: once DAP allocates validator/author budgets, redirect XCM execution fees to the
	// accumulation account instead of block author (use AccumulateForward as the OnUnbalanced
	// handler).
	type Trader =
		UsingComponents<WeightToFee, TokenLocation, AccountId, Balances, ToAuthor<Runtime>>;
	type ResponseHandler = XcmPallet;
	type AssetTrap = XcmPallet;
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = XcmPallet;
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = XcmFeeManagerFromComponents<
		WaivedLocations,
		SendXcmFeeToAccount<Self::AssetTransactor, AccumulateAccount>,
	>;
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = Aliasers;
	type TransactionalProcessor = FrameTransactionalProcessor;
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = XcmPallet;
}

/// Converts a local signed origin into an XCM location. Forms the basis for local origins
/// sending/executing XCMs.
pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, ThisNetwork>;

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = xcm_builder::EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	// Anyone can execute XCM messages locally.
	type ExecuteXcmOrigin = xcm_builder::EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Everything;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::WestendXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = IsConcrete<TokenLocation>;
	type TrustedLockers = ();
	type SovereignAccountOf = LocationConverter;
	type MaxLockers = ConstU32<8>;
	type MaxRemoteLockConsumers = ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = crate::weights::pallet_xcm::WeightInfo<Runtime>;
	type AdminOrigin = EnsureRoot<AccountId>;
	// Aliasing is disabled: xcm_executor::Config::Aliasers only allows `AliasChildLocation`.
	type AuthorizedAliasConsideration = Disabled;
}
