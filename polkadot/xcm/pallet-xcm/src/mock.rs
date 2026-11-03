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

pub use core::cell::RefCell;
use frame_support::{
	construct_runtime, derive_impl, parameter_types,
	traits::{
		fungible::HoldConsideration, AsEnsureOriginWithArg, ConstU128, ConstU32, Contains, Equals,
		Everything, EverythingBut, Footprint, Nothing,
	},
	weights::{
		constants::{WEIGHT_PROOF_SIZE_PER_MB, WEIGHT_REF_TIME_PER_SECOND},
		Weight,
	},
};
use frame_system::EnsureRoot;
use polkadot_parachain_primitives::primitives::Id as ParaId;
use polkadot_runtime_parachains::origin;
use sp_core::H256;
use sp_runtime::{
	traits::{Convert, Identity, IdentityLookup, TryConvertInto},
	AccountId32, BuildStorage,
};
use xcm::prelude::*;
use xcm_builder::{
	AccountId32Aliases, AllowKnownQueryResponses, AllowSubscriptionsFrom,
	AllowTopLevelPaidExecutionFrom, Case, ChildParachainAsNative, ChildParachainConvertsVia,
	ChildSystemParachainAsSuperuser, DescribeAllTerminal, EnsureDecodableXcm, FixedRateOfFungible,
	FixedWeightBounds, FrameTransactionalProcessor, FungibleAdapter, FungiblesAdapter,
	HashedDescription, IsConcrete, MatchedConvertedConcreteId, NoChecking, SendXcmFeeToAccount,
	SignedAccountId32AsNative, SignedToAccountId32, SovereignSignedViaLocation, TakeWeightCredit,
	XcmFeeManagerFromComponents,
};
use xcm_executor::XcmExecutor;
use xcm_simulator::helpers::derive_topic_id;

use crate::{self as pallet_xcm, TestWeightInfo};

pub type AccountId = AccountId32;
pub type Balance = u128;
type Block = frame_system::mocking::MockBlock<Test>;

#[frame_support::pallet]
pub mod pallet_test_notifier {
	use crate::{ensure_response, QueryId};
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_runtime::DispatchResult;
	use xcm::latest::prelude::*;
	use xcm_executor::traits::QueryHandler;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + crate::Config {
		#[allow(deprecated)]
		type RuntimeEvent: IsType<<Self as frame_system::Config>::RuntimeEvent> + From<Event<Self>>;
		type RuntimeOrigin: IsType<<Self as frame_system::Config>::RuntimeOrigin>
			+ Into<Result<crate::Origin, <Self as Config>::RuntimeOrigin>>;
		type RuntimeCall: IsType<<Self as crate::Config>::RuntimeCall> + From<Call<Self>>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		QueryPrepared(QueryId),
		NotifyQueryPrepared(QueryId),
		ResponseReceived(Location, QueryId, Response),
	}

	#[pallet::error]
	pub enum Error<T> {
		UnexpectedId,
		BadAccountFormat,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::from_parts(1_000_000, 1_000_000))]
		pub fn prepare_new_query(origin: OriginFor<T>, querier: Location) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let id = who
				.using_encoded(|mut d| <[u8; 32]>::decode(&mut d))
				.map_err(|_| Error::<T>::BadAccountFormat)?;
			let qid = <crate::Pallet<T> as QueryHandler>::new_query(
				Junction::AccountId32 { network: None, id },
				100u32.into(),
				querier,
			);
			Self::deposit_event(Event::<T>::QueryPrepared(qid));
			Ok(())
		}

		#[pallet::call_index(1)]
		#[pallet::weight(Weight::from_parts(1_000_000, 1_000_000))]
		pub fn prepare_new_notify_query(origin: OriginFor<T>, querier: Location) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let id = who
				.using_encoded(|mut d| <[u8; 32]>::decode(&mut d))
				.map_err(|_| Error::<T>::BadAccountFormat)?;
			let call =
				Call::<T>::notification_received { query_id: 0, response: Default::default() };
			let qid = crate::Pallet::<T>::new_notify_query(
				Junction::AccountId32 { network: None, id },
				<T as Config>::RuntimeCall::from(call),
				100u32.into(),
				querier,
			);
			Self::deposit_event(Event::<T>::NotifyQueryPrepared(qid));
			Ok(())
		}

		#[pallet::call_index(2)]
		#[pallet::weight(Weight::from_parts(1_000_000, 1_000_000))]
		pub fn notification_received(
			origin: OriginFor<T>,
			query_id: QueryId,
			response: Response,
		) -> DispatchResult {
			let responder = ensure_response(<T as Config>::RuntimeOrigin::from(origin))?;
			Self::deposit_event(Event::<T>::ResponseReceived(responder, query_id, response));
			Ok(())
		}
	}
}

construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		AssetsPallet: pallet_assets,
		ParasOrigin: origin,
		XcmPallet: pallet_xcm,
		TestNotifier: pallet_test_notifier,
		Utility: pallet_utility,
	}
);

thread_local! {
	pub static SENT_XCM: RefCell<Vec<(Location, Xcm<()>)>> = RefCell::new(Vec::new());
	pub static FAIL_SEND_XCM: RefCell<bool> = RefCell::new(false);
}
pub(crate) fn sent_xcm() -> Vec<(Location, Xcm<()>)> {
	SENT_XCM.with(|q| (*q.borrow()).clone())
}
pub(crate) fn take_sent_xcm() -> Vec<(Location, Xcm<()>)> {
	SENT_XCM.with(|q| {
		let mut r = Vec::new();
		std::mem::swap(&mut r, &mut *q.borrow_mut());
		r
	})
}
pub(crate) fn set_send_xcm_artificial_failure(should_fail: bool) {
	FAIL_SEND_XCM.with(|q| *q.borrow_mut() = should_fail);
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

parameter_types! {
	pub Para3000: u32 = 3000;
	pub Para3000Location: Location = Parachain(Para3000::get()).into();
	pub Para3000PaymentAmount: u128 = 1;
	pub Para3000PaymentAssets: Assets = Assets::from(Asset::from((Here, Para3000PaymentAmount::get())));
}
/// Sender only sends to `Parachain(3000)` destination requiring payment.
pub struct TestPaidForPara3000SendXcm;
impl SendXcm for TestPaidForPara3000SendXcm {
	type Ticket = (Location, Xcm<()>);
	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<(Location, Xcm<()>)> {
		if let Some(dest) = dest.as_ref() {
			if !dest.eq(&Para3000Location::get()) {
				return Err(SendError::NotApplicable);
			}
		} else {
			return Err(SendError::NotApplicable);
		}

		let pair = (dest.take().unwrap(), msg.take().unwrap());
		Ok((pair, Para3000PaymentAssets::get()))
	}
	fn deliver(pair: (Location, Xcm<()>)) -> Result<XcmHash, SendError> {
		let hash = derive_topic_id(&pair.1);
		SENT_XCM.with(|q| q.borrow_mut().push(pair));
		Ok(hash)
	}
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	type Nonce = u64;
	type Hash = H256;
	type Hashing = ::sp_runtime::traits::BlakeTwo256;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
	type Block = Block;
	type RuntimeEvent = RuntimeEvent;
	type BlockWeights = ();
	type BlockLength = ();
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = pallet_balances::AccountData<Balance>;
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type DbWeight = ();
	type BaseCallFilter = Everything;
	type SystemWeightInfo = ();
	type SS58Prefix = ();
	type OnSetCode = ();
	type MaxConsumers = frame_support::traits::ConstU32<16>;
}

parameter_types! {
	pub ExistentialDeposit: Balance = 1;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
}

// Lets the `weigh_message` benchmark build the same batched worst case real runtimes do;
// see `benchmarking::Config::batch_call`.
impl pallet_utility::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = ();
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

impl pallet_assets::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Balance = Balance;
	type AssetId = Location;
	type AssetIdParameter = Location;
	type ReserveData = ();
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
	type AssetDeposit = ConstU128<1>;
	type AssetAccountDeposit = ConstU128<10>;
	type MetadataDepositBase = ConstU128<1>;
	type MetadataDepositPerByte = ConstU128<1>;
	type ApprovalDeposit = ConstU128<1>;
	type StringLimit = ConstU32<50>;
	type Holder = ();
	type Freezer = ();
	type WeightInfo = ();
	type CallbackHandle = ();
	type AssetIdAllocator = ();
	type Extra = ();
	type RemoveItemsLimit = ConstU32<5>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = XcmBenchmarkHelper;
}

// This child parachain is a system parachain trusted to teleport native token.
pub const SOME_SYSTEM_PARA: u32 = 1001;

// This child parachain acts as trusted reserve for its assets in tests.
// USDT allowed to teleport to/from here.
pub const FOREIGN_ASSET_RESERVE_PARA_ID: u32 = 2001;
// Inner junction of reserve asset on `FOREIGN_ASSET_RESERVE_PARA_ID`.
pub const FOREIGN_ASSET_INNER_JUNCTION: Junction = GeneralIndex(1234567);

// This child parachain acts as trusted reserve for say.. USDC that can be used for fees.
pub const USDC_RESERVE_PARA_ID: u32 = 2002;
// Inner junction of reserve asset on `USDC_RESERVE_PARA_ID`.
pub const USDC_INNER_JUNCTION: Junction = PalletInstance(42);

// This child parachain is a trusted teleporter for say.. USDT (T from Teleport :)).
// We'll use USDT in tests that teleport fees.
pub const USDT_PARA_ID: u32 = 2003;

// This child parachain is not configured as trusted reserve or teleport location for any assets.
pub const OTHER_PARA_ID: u32 = 2009;

// This child parachain is used for filtered/disallowed assets.
pub const FILTERED_PARA_ID: u32 = 2010;

parameter_types! {
	pub const RelayLocation: Location = Here.into_location();
	pub const NativeAsset: Asset = Asset {
		fun: Fungible(10),
		id: AssetId(Here.into_location()),
	};
	pub SystemParachainLocation: Location = Location::new(
		0,
		[Parachain(SOME_SYSTEM_PARA)]
	);
	pub ForeignReserveLocation: Location = Location::new(
		0,
		[Parachain(FOREIGN_ASSET_RESERVE_PARA_ID)]
	);
	pub PaidParaForeignReserveLocation: Location = Location::new(
		0,
		[Parachain(Para3000::get())]
	);
	pub ForeignAsset: Asset = Asset {
		fun: Fungible(10),
		id: AssetId(Location::new(
			0,
			[Parachain(FOREIGN_ASSET_RESERVE_PARA_ID), FOREIGN_ASSET_INNER_JUNCTION],
		)),
	};
	pub PaidParaForeignAsset: Asset = Asset {
		fun: Fungible(10),
		id: AssetId(Location::new(
			0,
			[Parachain(Para3000::get())],
		)),
	};
	pub UsdcReserveLocation: Location = Location::new(
		0,
		[Parachain(USDC_RESERVE_PARA_ID)]
	);
	pub Usdc: Asset = Asset {
		fun: Fungible(10),
		id: AssetId(Location::new(
			0,
			[Parachain(USDC_RESERVE_PARA_ID), USDC_INNER_JUNCTION],
		)),
	};
	pub UsdtTeleportLocation: Location = Location::new(
		0,
		[Parachain(USDT_PARA_ID)]
	);
	pub Usdt: Asset = Asset {
		fun: Fungible(10),
		id: AssetId(Location::new(
			0,
			[Parachain(USDT_PARA_ID)],
		)),
	};
	pub FilteredTeleportLocation: Location = Location::new(
		0,
		[Parachain(FILTERED_PARA_ID)]
	);
	pub FilteredTeleportAsset: Asset = Asset {
		fun: Fungible(10),
		id: AssetId(Location::new(
			0,
			[Parachain(FILTERED_PARA_ID)],
		)),
	};
	pub const AnyNetwork: Option<NetworkId> = None;
	pub UniversalLocation: InteriorLocation = GlobalConsensus(ByGenesis([0; 32])).into();
	pub UnitWeightCost: u64 = 1_000;
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

type LocalOriginConverter = (
	SovereignSignedViaLocation<SovereignAccountOf, RuntimeOrigin>,
	ChildParachainAsNative<origin::Origin, RuntimeOrigin>,
	SignedAccountId32AsNative<AnyNetwork, RuntimeOrigin>,
	ChildSystemParachainAsSuperuser<ParaId, RuntimeOrigin>,
);

parameter_types! {
	pub const BaseXcmWeight: Weight = Weight::from_parts(1_000, 1_000);
	pub CurrencyPerSecondPerByte: (AssetId, u128, u128) = (AssetId(RelayLocation::get()), WEIGHT_REF_TIME_PER_SECOND.into(), WEIGHT_PROOF_SIZE_PER_MB.into());
	pub TrustedLocal: (AssetFilter, Location) = (All.into(), Here.into());
	pub TrustedSystemPara: (AssetFilter, Location) = (NativeAsset::get().into(), SystemParachainLocation::get());
	pub TrustedUsdt: (AssetFilter, Location) = (Usdt::get().into(), UsdtTeleportLocation::get());
	pub TrustedFilteredTeleport: (AssetFilter, Location) = (FilteredTeleportAsset::get().into(), FilteredTeleportLocation::get());
	pub TeleportUsdtToForeign: (AssetFilter, Location) = (Usdt::get().into(), ForeignReserveLocation::get());
	pub TrustedForeign: (AssetFilter, Location) = (ForeignAsset::get().into(), ForeignReserveLocation::get());
	pub TrustedPaidParaForeign: (AssetFilter, Location) = (PaidParaForeignAsset::get().into(), PaidParaForeignReserveLocation::get());

	pub TrustedUsdc: (AssetFilter, Location) = (Usdc::get().into(), UsdcReserveLocation::get());
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub XcmFeesTargetAccount: AccountId = AccountId::new([167u8; 32]);
}

pub const XCM_FEES_NOT_WAIVED_USER_ACCOUNT: [u8; 32] = [37u8; 32];

pub struct XcmFeesNotWaivedLocations;
impl Contains<Location> for XcmFeesNotWaivedLocations {
	fn contains(location: &Location) -> bool {
		matches!(
			location.unpack(),
			(0, [Junction::AccountId32 { network: None, id: XCM_FEES_NOT_WAIVED_USER_ACCOUNT }])
		)
	}
}

pub type Barrier = (
	TakeWeightCredit,
	AllowTopLevelPaidExecutionFrom<Everything>,
	AllowKnownQueryResponses<XcmPallet>,
	AllowSubscriptionsFrom<Everything>,
);

pub type XcmRouter =
	EnsureDecodableXcm<(TestPaidForPara3000SendXcm, TestSendXcmErrX8, TestSendXcm)>;

pub type Trader = FixedRateOfFungible<CurrencyPerSecondPerByte, ()>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = XcmPallet;
	type AssetTransactor = AssetTransactors;
	type OriginConverter = LocalOriginConverter;
	type IsReserve = (Case<TrustedForeign>, Case<TrustedUsdc>, Case<TrustedPaidParaForeign>);
	type IsTeleporter = (
		Case<TrustedLocal>,
		Case<TrustedSystemPara>,
		Case<TrustedUsdt>,
		Case<TeleportUsdtToForeign>,
		Case<TrustedFilteredTeleport>,
	);
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type Trader = Trader;
	type ResponseHandler = XcmPallet;
	type AssetTrap = XcmPallet;
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = XcmPallet;
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = XcmFeeManagerFromComponents<
		EverythingBut<XcmFeesNotWaivedLocations>,
		SendXcmFeeToAccount<Self::AssetTransactor, XcmFeesTargetAccount>,
	>;
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

/// Converts a local signed origin into an XCM location. Forms the basis for local origins
/// sending/executing XCMs.
pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, AnyNetwork>;

parameter_types! {
	pub static AdvertisedXcmVersion: pallet_xcm::XcmVersion = 4;
	pub const AuthorizeAliasHoldReason: RuntimeHoldReason = RuntimeHoldReason::XcmPallet(pallet_xcm::HoldReason::AuthorizeAlias);
}

pub struct ConvertDeposit;
impl Convert<Footprint, u128> for ConvertDeposit {
	fn convert(a: Footprint) -> u128 {
		(a.count * 2 + a.size) as u128
	}
}

pub struct XcmTeleportFiltered;
impl Contains<(Location, Vec<Asset>)> for XcmTeleportFiltered {
	fn contains(t: &(Location, Vec<Asset>)) -> bool {
		let filtered = FilteredTeleportAsset::get();
		t.1.iter().any(|asset| asset == &filtered)
	}
}

impl pallet_xcm::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = xcm_builder::EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = XcmRouter;
	type ExecuteXcmOrigin = xcm_builder::EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = EverythingBut<XcmTeleportFiltered>;
	type XcmReserveTransferFilter = Everything;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = AdvertisedXcmVersion;
	type AdminOrigin = EnsureRoot<AccountId>;
	type TrustedLockers = ();
	type SovereignAccountOf = AccountId32Aliases<(), AccountId32>;
	type Currency = Balances;
	type CurrencyMatcher = IsConcrete<RelayLocation>;
	type MaxLockers = frame_support::traits::ConstU32<8>;
	type MaxRemoteLockConsumers = frame_support::traits::ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = TestWeightInfo;
	type AuthorizedAliasConsideration =
		HoldConsideration<AccountId, Balances, AuthorizeAliasHoldReason, ConvertDeposit>;
}

impl origin::Config for Test {}

impl pallet_test_notifier::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct TestDeliveryHelper;
#[cfg(feature = "runtime-benchmarks")]
impl xcm_builder::EnsureDelivery for TestDeliveryHelper {
	fn ensure_successful_delivery(
		origin_ref: &Location,
		_dest: &Location,
		_fee_reason: xcm_executor::traits::FeeReason,
	) -> (Option<xcm_executor::FeesMode>, Option<Assets>) {
		use xcm_executor::traits::ConvertLocation;
		let account = SovereignAccountOf::convert_location(origin_ref).expect("Valid location");
		// Give the existential deposit at least
		let balance = ExistentialDeposit::get();
		let _ = <Balances as frame_support::traits::Currency<_>>::make_free_balance_be(
			&account, balance,
		);
		(None, None)
	}
}

#[cfg(feature = "runtime-benchmarks")]
impl super::benchmarking::Config for Test {
	type DeliveryHelper = TestDeliveryHelper;

	fn reachable_dest() -> Option<Location> {
		Some(Parachain(1000).into())
	}

	fn teleportable_asset_and_dest() -> Option<(Asset, Location)> {
		Some((NativeAsset::get(), SystemParachainLocation::get()))
	}

	fn reserve_transferable_asset_and_dest() -> Option<(Asset, Location)> {
		Some((
			Asset { fun: Fungible(10), id: AssetId(Here.into_location()) },
			Parachain(OTHER_PARA_ID).into(),
		))
	}

	fn set_up_complex_asset_transfer() -> Option<(Assets, u32, Location, Box<dyn FnOnce()>)> {
		use crate::tests::assets_transfer::{into_assets_checked, set_up_foreign_asset};
		// Transfer native asset (local reserve) to `USDT_PARA_ID`. Using teleport-trusted USDT for
		// fees.

		let asset_amount = 10u128;
		let fee_amount = 2u128;

		let existential_deposit = ExistentialDeposit::get();
		let caller = frame_benchmarking::whitelisted_caller();

		// Give some multiple of the existential deposit
		let balance = asset_amount + existential_deposit * 1000;
		let _ = <Balances as frame_support::traits::Currency<_>>::make_free_balance_be(
			&caller, balance,
		);
		// create sufficient foreign asset USDT
		let usdt_initial_local_amount = fee_amount * 10;
		let (usdt_chain, _, usdt_id_location) = set_up_foreign_asset(
			USDT_PARA_ID,
			None,
			caller.clone(),
			usdt_initial_local_amount,
			true,
		);

		// native assets transfer destination is USDT chain (teleport trust only for USDT)
		let dest = usdt_chain;
		let (assets, fee_index, _, _) = into_assets_checked(
			// USDT for fees (is sufficient on local chain too) - teleported
			(usdt_id_location.clone(), fee_amount).into(),
			// native asset to transfer (not used for fees) - local reserve
			(Location::here(), asset_amount).into(),
		);
		// verify initial balances
		assert_eq!(Balances::free_balance(&caller), balance);
		assert_eq!(
			AssetsPallet::balance(usdt_id_location.clone(), &caller),
			usdt_initial_local_amount
		);

		// verify transferred successfully
		let verify = Box::new(move || {
			// verify balances after transfer, decreased by transferred amounts
			assert_eq!(Balances::free_balance(&caller), balance - asset_amount);
			assert_eq!(
				AssetsPallet::balance(usdt_id_location, &caller),
				usdt_initial_local_amount - fee_amount
			);
		});
		Some((assets, fee_index as u32, dest, verify))
	}

	fn get_asset() -> Asset {
		Asset { id: AssetId(Location::here()), fun: Fungible(ExistentialDeposit::get()) }
	}

	fn get_assets(n: u32) -> Assets {
		let owner: AccountId = frame_benchmarking::whitelisted_caller();
		let mut assets: Vec<Asset> = vec![Self::get_asset()];
		for i in 1..n {
			// Distinct, depositable foreign assets handled by the `FungiblesAdapter`.
			let asset_id_location = Location::new(0, [Parachain(2_000 + i)]);
			frame_support::assert_ok!(AssetsPallet::force_create(
				RuntimeOrigin::root(),
				asset_id_location.clone(),
				owner.clone(),
				true, // sufficient, so the beneficiary needs no provider reference
				1,
			));
			assets.push(Asset { id: AssetId(asset_id_location), fun: Fungible(100) });
		}
		assets.into()
	}

	fn batch_call(calls: Vec<RuntimeCall>) -> Option<RuntimeCall> {
		Some(RuntimeCall::Utility(pallet_utility::Call::batch { calls }))
	}
}

pub(crate) fn all_events() -> Vec<RuntimeEvent> {
	System::events().into_iter().map(|e| e.event).collect()
}

pub(crate) fn last_events(n: usize) -> Vec<RuntimeEvent> {
	let all_events = all_events();
	let split_idx = all_events.len().saturating_sub(n);
	all_events.split_at(split_idx).1.to_vec()
}

pub(crate) fn last_event() -> RuntimeEvent {
	last_events(1).pop().expect("RuntimeEvent expected")
}

pub(crate) fn buy_execution<C>(fees: impl Into<Asset>) -> Instruction<C> {
	use xcm::latest::prelude::*;
	BuyExecution { fees: fees.into(), weight_limit: Unlimited }
}

pub(crate) fn buy_limited_execution<C>(
	fees: impl Into<Asset>,
	weight_limit: WeightLimit,
) -> Instruction<C> {
	use xcm::latest::prelude::*;
	BuyExecution { fees: fees.into(), weight_limit }
}

pub(crate) fn new_test_ext_with_balances(
	balances: Vec<(AccountId, Balance)>,
) -> sp_io::TestExternalities {
	new_test_ext_with_balances_and_xcm_version(
		balances,
		// By default set actual latest XCM version
		Some(XCM_VERSION),
		vec![],
	)
}

pub(crate) fn new_test_ext_with_balances_and_xcm_version(
	balances: Vec<(AccountId, Balance)>,
	safe_xcm_version: Option<XcmVersion>,
	supported_version: Vec<(Location, XcmVersion)>,
) -> sp_io::TestExternalities {
	sp_tracing::try_init_simple();
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<Test> { balances, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	pallet_xcm::GenesisConfig::<Test> { safe_xcm_version, supported_version, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}
