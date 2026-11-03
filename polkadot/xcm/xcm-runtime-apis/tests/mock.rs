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

//! Mock runtime for tests.
//! Implements both runtime APIs for fee estimation and getting the messages for transfers.

use core::{cell::RefCell, marker::PhantomData};
use frame_support::{
	construct_runtime, derive_impl, parameter_types, sp_runtime,
	sp_runtime::{
		traits::{Get, IdentityLookup, MaybeEquivalence, TryConvert, TryConvertInto},
		BuildStorage, SaturatedConversion,
	},
	traits::{
		AsEnsureOriginWithArg, ConstU128, ConstU32, Contains, ContainsPair, Disabled, Everything,
		Nothing, OriginTrait,
	},
	weights::WeightToFee as WeightToFeeT,
};
use frame_system::{EnsureRoot, RawOrigin as SystemRawOrigin};
use pallet_xcm::TestWeightInfo;
use xcm::{prelude::*, Version as XcmVersion};
use xcm_builder::{
	AllowTopLevelPaidExecutionFrom, ConvertedConcreteId, EnsureXcmOrigin, FixedRateOfFungible,
	FixedWeightBounds, FungibleAdapter, FungiblesAdapter, InspectMessageQueues, IsConcrete,
	MintLocation, NoChecking, TakeWeightCredit,
};
use xcm_executor::{traits::ConvertLocation, XcmExecutor};

use xcm_runtime_apis::{
	conversions::{Error as LocationToAccountApiError, LocationToAccountApi},
	dry_run::{CallDryRunEffects, DryRunApi, Error as XcmDryRunApiError, XcmDryRunEffects},
	fees::{Error as XcmPaymentApiError, XcmPaymentApi},
	trusted_query::{Error as TrustedQueryApiError, TrustedQueryApi},
};
use xcm_simulator::helpers::derive_topic_id;

construct_runtime! {
	pub enum TestRuntime {
		System: frame_system,
		Balances: pallet_balances,
		AssetsPallet: pallet_assets,
		XcmPallet: pallet_xcm,
	}
}

pub type TxExtension =
	(frame_system::CheckWeight<TestRuntime>, frame_system::WeightReclaim<TestRuntime>);

// we only use the hash type from this, so using the mock should be fine.
pub(crate) type Extrinsic = sp_runtime::generic::UncheckedExtrinsic<
	u64,
	RuntimeCall,
	sp_runtime::testing::UintAuthorityId,
	TxExtension,
>;
type Block = sp_runtime::testing::Block<Extrinsic>;
type Balance = u128;
type AssetIdForAssetsPallet = u32;
type AccountId = u64;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for TestRuntime {
	type Block = Block;
	type AccountId = AccountId;
	type AccountData = pallet_balances::AccountData<Balance>;
	type Lookup = IdentityLookup<AccountId>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for TestRuntime {
	type AccountStore = System;
	type Balance = Balance;
	type ExistentialDeposit = ExistentialDeposit;
}

// Assets instance
#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config for TestRuntime {
	type AssetId = AssetIdForAssetsPallet;
	type Balance = Balance;
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId>>;
	type ForceOrigin = frame_system::EnsureRoot<AccountId>;
	type Holder = ();
	type Freezer = ();
	type AssetDeposit = ConstU128<1>;
	type AssetAccountDeposit = ConstU128<10>;
	type MetadataDepositBase = ConstU128<1>;
	type MetadataDepositPerByte = ConstU128<1>;
	type ApprovalDeposit = ConstU128<1>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

thread_local! {
	pub static SENT_XCM: RefCell<Vec<(Location, Xcm<()>)>> = const { RefCell::new(Vec::new()) };
}

pub struct TestXcmSender;
impl SendXcm for TestXcmSender {
	type Ticket = (Location, Xcm<()>);
	fn validate(
		dest: &mut Option<Location>,
		msg: &mut Option<Xcm<()>>,
	) -> SendResult<Self::Ticket> {
		let ticket = (dest.take().unwrap(), msg.take().unwrap());
		let fees: Assets = (HereLocation::get(), DeliveryFees::get()).into();
		Ok((ticket, fees))
	}
	fn deliver(ticket: Self::Ticket) -> Result<XcmHash, SendError> {
		let hash = derive_topic_id(&ticket.1);
		SENT_XCM.with(|q| q.borrow_mut().push(ticket));
		Ok(hash)
	}
}
impl InspectMessageQueues for TestXcmSender {
	fn clear_messages() {
		SENT_XCM.with(|q| q.borrow_mut().clear());
	}

	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		SENT_XCM.with(|q| {
			(*q.borrow())
				.clone()
				.iter()
				.map(|(location, message)| {
					(
						VersionedLocation::from(location.clone()),
						vec![VersionedXcm::from(message.clone())],
					)
				})
				.collect()
		})
	}
}

pub type XcmRouter = TestXcmSender;

parameter_types! {
	pub const DeliveryFees: u128 = 20; // Arbitrary value.
	pub const ExistentialDeposit: u128 = 1; // Arbitrary value.
	pub const BaseXcmWeight: Weight = Weight::from_parts(100, 10); // Arbitrary value.
	pub const MaxInstructions: u32 = 100;
	pub const NativeTokenPerSecondPerByte: (AssetId, u128, u128) = (AssetId(HereLocation::get()), 1, 1);
	pub UniversalLocation: InteriorLocation = [GlobalConsensus(NetworkId::ByGenesis([0; 32])), Parachain(2000)].into();
	pub const HereLocation: Location = Location::here();
	pub const RelayLocation: Location = Location::parent();
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub CheckAccount: AccountId = XcmPallet::check_account();
	pub LocalCheckAccount: (AccountId, MintLocation) = (CheckAccount::get(), MintLocation::Local);
	pub const AnyNetwork: Option<NetworkId> = None;
}

/// Simple `WeightToFee` implementation that adds the ref_time by the proof_size.
pub struct WeightToFee;
impl WeightToFeeT for WeightToFee {
	type Balance = Balance;
	fn weight_to_fee(weight: &Weight) -> Self::Balance {
		Self::Balance::saturated_from(weight.ref_time())
			.saturating_add(Self::Balance::saturated_from(weight.proof_size()))
	}
}

type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;

/// Matches the pair (NativeToken, AssetHub).
/// This is used in the `IsTeleporter` configuration item, meaning we accept our native token
/// coming from AssetHub as a teleport.
pub struct NativeTokenToAssetHub;
impl ContainsPair<Asset, Location> for NativeTokenToAssetHub {
	fn contains(asset: &Asset, origin: &Location) -> bool {
		matches!(asset.id.0.unpack(), (0, [])) &&
			matches!(origin.unpack(), (1, [Parachain(ASSET_HUB_PARA_ID)]))
	}
}

/// Parachain id of Asset Hub.
pub const ASSET_HUB_PARA_ID: u32 = 1000;
/// The instance index of the trust-backed assets pallet in Asset Hub.
pub const ASSET_HUB_ASSETS_PALLET_INSTANCE: u8 = 50;
/// Id of USDT in Asset Hub.
pub const USDT_ID: u32 = 1984;

/// Matches the pairs (RelayToken, AssetHub) and (UsdtToken, AssetHub).
/// This is used in the `IsReserve` configuration item, meaning we accept the relay token
/// coming from AssetHub as a reserve asset transfer.
pub struct RelayTokenAndUsdtToAssetHub;
impl ContainsPair<Asset, Location> for RelayTokenAndUsdtToAssetHub {
	fn contains(asset: &Asset, origin: &Location) -> bool {
		let is_asset_hub = matches!(origin.unpack(), (1, [Parachain(ASSET_HUB_PARA_ID)]));
		let is_relay_token = matches!(asset.id.0.unpack(), (1, []));
		let is_usdt = matches!(asset.id.0.unpack(), (1, [Parachain(ASSET_HUB_PARA_ID), PalletInstance(ASSET_HUB_ASSETS_PALLET_INSTANCE), GeneralIndex(asset_id)]) if *asset_id == USDT_ID.into());

		is_asset_hub && (is_relay_token || is_usdt)
	}
}

/// Converts locations that are only the `AccountIndex64` junction into local u64 accounts.
pub struct AccountIndex64Aliases<Network, AccountId>(PhantomData<(Network, AccountId)>);
impl<Network: Get<Option<NetworkId>>, AccountId: From<u64>> ConvertLocation<AccountId>
	for AccountIndex64Aliases<Network, AccountId>
{
	fn convert_location(location: &Location) -> Option<AccountId> {
		let index = match location.unpack() {
			(0, [AccountIndex64 { index, network: None }]) => index,
			(0, [AccountIndex64 { index, network }]) if *network == Network::get() => index,
			_ => return None,
		};
		Some((*index).into())
	}
}

/// Custom location converter to turn sibling chains into u64 accounts.
pub struct SiblingChainToIndex64;
impl ConvertLocation<AccountId> for SiblingChainToIndex64 {
	fn convert_location(location: &Location) -> Option<AccountId> {
		let index = match location.unpack() {
			(1, [Parachain(id)]) => id,
			_ => return None,
		};
		Some((*index).into())
	}
}

/// We alias local account locations to actual local accounts.
/// We also allow sovereign accounts for other sibling chains.
pub type LocationToAccountId = (AccountIndex64Aliases<AnyNetwork, u64>, SiblingChainToIndex64);

pub type NativeTokenTransactor = FungibleAdapter<
	// We use pallet-balances for handling this fungible asset.
	Balances,
	// The fungible asset handled by this transactor is the native token of the chain.
	IsConcrete<HereLocation>,
	// How we convert locations to accounts.
	LocationToAccountId,
	// We need to specify the AccountId type.
	AccountId,
	// We mint the native tokens locally, so we track how many we've sent away via teleports.
	LocalCheckAccount,
>;

/// Converter from Location to local asset id and viceversa.
pub struct LocationToAssetIdForAssetsPallet;
impl MaybeEquivalence<Location, AssetIdForAssetsPallet> for LocationToAssetIdForAssetsPallet {
	fn convert(location: &Location) -> Option<AssetIdForAssetsPallet> {
		match location.unpack() {
			(1, []) => Some(1 as AssetIdForAssetsPallet),
			(
				1,
				[Parachain(ASSET_HUB_PARA_ID), PalletInstance(ASSET_HUB_ASSETS_PALLET_INSTANCE), GeneralIndex(asset_id)],
			) if *asset_id == USDT_ID.into() => Some(USDT_ID as AssetIdForAssetsPallet),
			_ => None,
		}
	}

	fn convert_back(id: &AssetIdForAssetsPallet) -> Option<Location> {
		match id {
			1 => Some(Location::new(1, [])),
			asset_id if *asset_id == USDT_ID => Some(Location::new(
				1,
				[
					Parachain(ASSET_HUB_PARA_ID),
					PalletInstance(ASSET_HUB_ASSETS_PALLET_INSTANCE),
					GeneralIndex(USDT_ID.into()),
				],
			)),
			_ => None,
		}
	}
}

/// AssetTransactor for handling assets other than the native one.
pub type AssetsTransactor = FungiblesAdapter<
	// We use pallet-assets for handling the relay token.
	AssetsPallet,
	// Matches the relay token.
	ConvertedConcreteId<
		AssetIdForAssetsPallet,
		Balance,
		LocationToAssetIdForAssetsPallet,
		TryConvertInto,
	>,
	// How we convert locations to accounts.
	LocationToAccountId,
	// We need to specify the AccountId type.
	AccountId,
	// We don't track teleports.
	NoChecking,
	(),
>;

pub type AssetTransactors = (NativeTokenTransactor, AssetsTransactor);

pub struct HereAndInnerLocations;
impl Contains<Location> for HereAndInnerLocations {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (0, []) | (0, _))
	}
}

pub type Barrier = (
	TakeWeightCredit, // We need this for pallet-xcm's extrinsics to work.
	AllowTopLevelPaidExecutionFrom<HereAndInnerLocations>, /* TODO: Technically, we should allow
	                   * messages from "AssetHub". */
);

pub type Trader = FixedRateOfFungible<NativeTokenPerSecondPerByte, ()>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type XcmEventEmitter = XcmPallet;
	type AssetTransactor = AssetTransactors;
	type OriginConverter = ();
	type IsReserve = RelayTokenAndUsdtToAssetHub;
	type IsTeleporter = NativeTokenToAssetHub;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = Weigher;
	type Trader = Trader;
	type ResponseHandler = ();
	type AssetTrap = ();
	type AssetLocker = ();
	type AssetExchanger = MockAssetExchanger;
	type SubscriptionService = ();
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = ();
	type MessageExporter = ();
	type UniversalAliases = ();
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Nothing;
	type Aliasers = Nothing;
	type TransactionalProcessor = ();
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = XcmPallet;
}

/// Mock AssetExchanger that recognizes USDT and provides a 1:2 exchange rate
/// (1 native token = 2 USDT tokens)
pub struct MockAssetExchanger;
impl xcm_executor::traits::AssetExchange for MockAssetExchanger {
	fn exchange_asset(
		_origin: Option<&Location>,
		give: xcm_executor::AssetsInHolding,
		want: &Assets,
		_maximal: bool,
	) -> Result<xcm_executor::AssetsInHolding, xcm_executor::AssetsInHolding> {
		let usdt_location = Location::new(
			1,
			[
				Parachain(ASSET_HUB_PARA_ID),
				PalletInstance(ASSET_HUB_ASSETS_PALLET_INSTANCE),
				GeneralIndex(USDT_ID.into()),
			],
		);

		// Note: With the new imbalance accounting system, creating arbitrary AssetsInHolding
		// for test purposes requires proper credit/debit tracking which is complex.
		// For now, this mock exchanger just returns the original assets (no exchange performed).
		// If tests need actual exchange logic, they should be updated to use proper pallet
		// operations that create valid imbalances.

		// Check if exchange would be supported
		if let Some(_give_asset) = give.fungible.get(&AssetId(HereLocation::get())) {
			if let Some(want_asset) = want.get(0) {
				if want_asset.id.0 == usdt_location {
					// Would exchange at 1:2 rate, but can't create proper AssetsInHolding
					// without real imbalances from pallet operations
					return Err(give);
				}
			}
		}

		// Can't handle the exchange, return the original assets
		Err(give)
	}

	fn quote_exchange_price(give: &Assets, want: &Assets, _maximal: bool) -> Option<Assets> {
		let usdt_location = Location::new(
			1,
			[
				Parachain(ASSET_HUB_PARA_ID),
				PalletInstance(ASSET_HUB_ASSETS_PALLET_INSTANCE),
				GeneralIndex(USDT_ID.into()),
			],
		);

		// Check if we're trying to quote native asset for USDT
		if let Some(give_asset) = give.get(0) {
			if let Some(want_asset) = want.get(0) {
				if give_asset.id.0 == HereLocation::get() && want_asset.id.0 == usdt_location {
					if let Fungible(amount) = give_asset.fun {
						// Return the USDT amount we'd get at 1:2 rate
						let usdt_amount = amount.saturating_mul(2);
						return Some((AssetId(usdt_location), usdt_amount).into());
					}
				}
			}
		}

		None
	}
}

/// Converts a signed origin of a u64 account into a location with only the `AccountIndex64`
/// junction.
pub struct SignedToAccountIndex64<RuntimeOrigin, AccountId>(
	PhantomData<(RuntimeOrigin, AccountId)>,
);
impl<RuntimeOrigin: OriginTrait + Clone, AccountId: Into<u64>> TryConvert<RuntimeOrigin, Location>
	for SignedToAccountIndex64<RuntimeOrigin, AccountId>
where
	RuntimeOrigin::PalletsOrigin: From<SystemRawOrigin<AccountId>>
		+ TryInto<SystemRawOrigin<AccountId>, Error = RuntimeOrigin::PalletsOrigin>,
{
	fn try_convert(origin: RuntimeOrigin) -> Result<Location, RuntimeOrigin> {
		origin.try_with_caller(|caller| match caller.try_into() {
			Ok(SystemRawOrigin::Signed(who)) => {
				Ok(Junction::AccountIndex64 { network: None, index: who.into() }.into())
			},
			Ok(other) => Err(other.into()),
			Err(other) => Err(other),
		})
	}
}

/// Converts a local signed origin into an XCM location. Forms the basis for local origins
/// sending/executing XCMs.
pub type LocalOriginToLocation = SignedToAccountIndex64<RuntimeOrigin, AccountId>;

impl pallet_xcm::Config for TestRuntime {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, ()>;
	type XcmRouter = XcmRouter;
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Nothing;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything; // Put everything instead of something more restricted.
	type XcmReserveTransferFilter = Everything; // Same.
	type Weigher = Weigher;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type AdminOrigin = EnsureRoot<AccountId>;
	type TrustedLockers = ();
	type SovereignAccountOf = ();
	type Currency = Balances;
	type CurrencyMatcher = IsConcrete<HereLocation>;
	type MaxLockers = ConstU32<0>;
	type MaxRemoteLockConsumers = ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = TestWeightInfo;
	type AuthorizedAliasConsideration = Disabled;
}

#[allow(dead_code)]
pub fn new_test_ext_with_balances(balances: Vec<(AccountId, Balance)>) -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<TestRuntime>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<TestRuntime> { balances, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

#[allow(dead_code)]
pub fn new_test_ext_with_balances_and_assets(
	balances: Vec<(AccountId, Balance)>,
	assets: Vec<(AssetIdForAssetsPallet, AccountId, Balance)>,
) -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<TestRuntime>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<TestRuntime> { balances, ..Default::default() }
		.assimilate_storage(&mut t)
		.unwrap();

	pallet_assets::GenesisConfig::<TestRuntime> {
		assets: vec![
			// id, owner, is_sufficient, min_balance.
			// We don't actually need this to be sufficient, since we use the native assets in
			// tests for the existential deposit.
			(1, 0, true, 1),
			(1984, 0, true, 1),
		],
		metadata: vec![
			// id, name, symbol, decimals.
			(1, "Relay Token".into(), "RLY".into(), 12),
			(1984, "Tether".into(), "USDT".into(), 6),
		],
		accounts: assets,
		next_asset_id: None,
		reserves: vec![],
	}
	.assimilate_storage(&mut t)
	.unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct TestClient;

#[allow(dead_code)]
pub(crate) struct RuntimeApi {
	_inner: TestClient,
}

impl sp_api::ProvideRuntimeApi<Block> for TestClient {
	type Api = RuntimeApi;
	fn runtime_api(&self) -> sp_api::ApiRef<'_, Self::Api> {
		RuntimeApi { _inner: self.clone() }.into()
	}
}

sp_api::mock_impl_runtime_apis! {
	impl TrustedQueryApi<Block> for RuntimeApi {
		fn is_trusted_reserve(asset: VersionedAsset, location: VersionedLocation) -> Result<bool, TrustedQueryApiError> {
			XcmPallet::is_trusted_reserve(asset, location)
		}

		fn is_trusted_teleporter(asset: VersionedAsset, location: VersionedLocation) -> Result<bool, TrustedQueryApiError> {
			XcmPallet::is_trusted_teleporter(asset, location)
		}
	}

	impl LocationToAccountApi<Block, AccountId> for RuntimeApi {
		fn convert_location(location: VersionedLocation) -> Result<AccountId, LocationToAccountApiError> {
			let location = location.try_into().map_err(|_| LocationToAccountApiError::VersionedConversionFailed)?;
			LocationToAccountId::convert_location(&location)
				.ok_or(LocationToAccountApiError::Unsupported)
		}
	}

	impl XcmPaymentApi<Block> for RuntimeApi {
		fn query_acceptable_payment_assets(xcm_version: XcmVersion) -> Result<Vec<VersionedAssetId>, XcmPaymentApiError> {
			Ok(vec![
				VersionedAssetId::from(AssetId(HereLocation::get()))
					.into_version(xcm_version)
					.map_err(|_| XcmPaymentApiError::VersionedConversionFailed)?
			])
		}

		fn query_xcm_weight(message: VersionedXcm<()>) -> Result<Weight, XcmPaymentApiError> {
			XcmPallet::query_xcm_weight(message)
		}

		fn query_weight_to_asset_fee(weight: Weight, asset: VersionedAssetId) -> Result<u128, XcmPaymentApiError> {
			let latest_asset_id: Result<AssetId, ()> = asset.clone().try_into();
			match latest_asset_id {
				Ok(asset_id) if asset_id.0 == HereLocation::get() => {
					Ok(WeightToFee::weight_to_fee(&weight))
				},
				Ok(asset_id) => {
					tracing::trace!(
						target: "xcm::XcmPaymentApi::query_weight_to_asset_fee",
						?asset_id,
						"query_weight_to_asset_fee - unhandled!"
					);
					Err(XcmPaymentApiError::AssetNotFound)
				},
				Err(_) => {
					tracing::trace!(
						target: "xcm::XcmPaymentApi::query_weight_to_asset_fee",
						?asset,
						"query_weight_to_asset_fee - failed to convert!"
					);
					Err(XcmPaymentApiError::VersionedConversionFailed)
				}
			}
		}

		fn query_delivery_fees(destination: VersionedLocation, message: VersionedXcm<()>, asset_id: VersionedAssetId) -> Result<VersionedAssets, XcmPaymentApiError> {
			XcmPallet::query_delivery_fees::<<XcmConfig as xcm_executor::Config>::AssetExchanger>(destination, message, asset_id)
		}
	}

	impl DryRunApi<Block, RuntimeCall, RuntimeEvent, OriginCaller> for RuntimeApi {
		fn dry_run_call(
			origin: OriginCaller,
			call: RuntimeCall,
			result_xcms_version: XcmVersion,
		) -> Result<CallDryRunEffects<RuntimeEvent>, XcmDryRunApiError> {
			pallet_xcm::Pallet::<TestRuntime>::dry_run_call::<TestRuntime, XcmRouter, OriginCaller, RuntimeCall>(origin, call, result_xcms_version)
		}

		fn dry_run_call_before_version_2(
			origin: OriginCaller,
			call: RuntimeCall,
		) -> Result<CallDryRunEffects<RuntimeEvent>, XcmDryRunApiError> {
			pallet_xcm::Pallet::<TestRuntime>::dry_run_call::<TestRuntime, XcmRouter, OriginCaller, RuntimeCall>(origin, call, xcm::latest::VERSION)
		}

		fn dry_run_xcm(origin_location: VersionedLocation, xcm: VersionedXcm<RuntimeCall>) -> Result<XcmDryRunEffects<RuntimeEvent>, XcmDryRunApiError> {
			pallet_xcm::Pallet::<TestRuntime>::dry_run_xcm::<XcmRouter>(origin_location, xcm)
		}
	}
}
