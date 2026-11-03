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

use super::*;

use codec::Decode;

use frame_support::{
	construct_runtime, derive_impl, parameter_types,
	traits::{AsEnsureOriginWithArg, ConstU32, Disabled, Everything, Nothing},
};
use frame_system::{EnsureRoot, EnsureSigned};
use polkadot_primitives::{AccountIndex, BlakeTwo256, Signature};
use sp_runtime::{
	generic,
	traits::{MaybeEquivalence, TryConvertInto},
	AccountId32, BuildStorage,
};
use xcm_executor::{traits::ConvertLocation, XcmExecutor};
use xcm_simulator::ParaId;

pub type TxExtension = (
	frame_system::AuthorizeCall<Test>,
	frame_system::CheckNonZeroSender<Test>,
	frame_system::CheckSpecVersion<Test>,
	frame_system::CheckTxVersion<Test>,
	frame_system::CheckGenesis<Test>,
	frame_system::CheckMortality<Test>,
	frame_system::CheckNonce<Test>,
	frame_system::CheckWeight<Test>,
	frame_system::WeightReclaim<Test>,
);
pub type Address = sp_runtime::MultiAddress<AccountId, AccountIndex>;
pub type UncheckedExtrinsic =
	generic::UncheckedExtrinsic<Address, RuntimeCall, Signature, TxExtension>;
pub type Header = generic::Header<BlockNumber, BlakeTwo256>;
pub type Block = generic::Block<Header, UncheckedExtrinsic>;

pub type BlockNumber = u32;
pub type AccountId = AccountId32;

construct_runtime!(
	pub enum Test {
		System: frame_system,
		Balances: pallet_balances,
		Assets: pallet_assets,
		Salary: pallet_salary,
		XcmPallet: pallet_xcm,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
	type AccountId = AccountId;
	type Lookup = sp_runtime::traits::IdentityLookup<AccountId>;
}

pub type Balance = u128;

parameter_types! {
	pub const ExistentialDeposit: Balance = 1;
}

impl pallet_balances::Config for Test {
	type MaxLocks = ConstU32<0>;
	type Balance = Balance;
	type RuntimeEvent = RuntimeEvent;
	type DustRemoval = ();
	type ExistentialDeposit = ExistentialDeposit;
	type AccountStore = System;
	type WeightInfo = ();
	type MaxReserves = ();
	type ReserveIdentifier = [u8; 8];
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = ();
	type MaxFreezes = ConstU32<0>;
	type DoneSlashHandler = ();
}

parameter_types! {
	pub const AssetDeposit: u128 = 1_000_000;
	pub const MetadataDepositBase: u128 = 1_000_000;
	pub const MetadataDepositPerByte: u128 = 100_000;
	pub const AssetAccountDeposit: u128 = 1_000_000;
	pub const ApprovalDeposit: u128 = 1_000_000;
	pub const AssetsStringLimit: u32 = 50;
	pub const RemoveItemsLimit: u32 = 50;
}

impl pallet_assets::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type Balance = Balance;
	type AssetId = AssetIdForAssets;
	type ReserveData = ();
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
	type AssetDeposit = AssetDeposit;
	type MetadataDepositBase = MetadataDepositBase;
	type MetadataDepositPerByte = MetadataDepositPerByte;
	type AssetAccountDeposit = AssetAccountDeposit;
	type ApprovalDeposit = ApprovalDeposit;
	type StringLimit = AssetsStringLimit;
	type Holder = ();
	type Freezer = ();
	type Extra = ();
	type WeightInfo = ();
	type RemoveItemsLimit = RemoveItemsLimit;
	type AssetIdParameter = AssetIdForAssets;
	type CallbackHandle = ();
	type AssetIdAllocator = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = ();
}

parameter_types! {
	pub const RelayLocation: Location = Location::parent();
	pub const AnyNetwork: Option<NetworkId> = None;
	pub MockRuntimeParachainId: ParaId = 42u32.into();
	pub UniversalLocation: InteriorLocation = (ByGenesis([0; 32]), Parachain(MockRuntimeParachainId::get().into())).into();
	pub UnitWeightCost: u64 = 1_000;
	pub const BaseXcmWeight: Weight = Weight::from_parts(1_000, 1_000);
	pub CurrencyPerSecondPerByte: (AssetId, u128, u128) = (AssetId(RelayLocation::get()), 1, 1);
	pub TrustedAssets: (AssetFilter, Location) = (All.into(), Here.into());
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub CheckingAccount: AccountId = XcmPallet::check_account();
}

/// Type representing both a location and an asset that is held at that location.
/// The id of the held asset is relative to the location where it is being held.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug)]
pub struct AssetKind {
	pub destination: Location,
	pub asset_id: AssetId,
}

pub struct LocatableAssetKindConverter;
impl sp_runtime::traits::TryConvert<AssetKind, LocatableAssetId> for LocatableAssetKindConverter {
	fn try_convert(value: AssetKind) -> Result<LocatableAssetId, AssetKind> {
		Ok(LocatableAssetId { asset_id: value.asset_id, location: value.destination })
	}
}

type AssetIdForAssets = u128;

pub struct FromLocationToAsset<Location, AssetId>(core::marker::PhantomData<(Location, AssetId)>);
impl MaybeEquivalence<Location, AssetIdForAssets>
	for FromLocationToAsset<Location, AssetIdForAssets>
{
	fn convert(value: &Location) -> Option<AssetIdForAssets> {
		match value.unpack() {
			(0, []) => Some(0 as AssetIdForAssets),
			(1, []) => Some(1 as AssetIdForAssets),
			(0, [PalletInstance(1), GeneralIndex(index)]) if ![0, 1].contains(index) => {
				Some(*index as AssetIdForAssets)
			},
			_ => None,
		}
	}

	fn convert_back(value: &AssetIdForAssets) -> Option<Location> {
		match value {
			0u128 => Some(Location { parents: 1, interior: Here }),
			1u128 => Some(Location { parents: 0, interior: Here }),
			para_id @ 1..=1000 => {
				Some(Location { parents: 1, interior: [Parachain(*para_id as u32)].into() })
			},
			_ => None,
		}
	}
}

/// Converts a local signed origin into an XCM location. Forms the basis for local origins
/// sending/executing XCMs.
pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, AnyNetwork>;
pub type LocalAssetsTransactor = FungiblesAdapter<
	Assets,
	ConvertedConcreteId<
		AssetIdForAssets,
		Balance,
		FromLocationToAsset<Location, AssetIdForAssets>,
		TryConvertInto,
	>,
	SovereignAccountOf,
	AccountId,
	NoChecking,
	CheckingAccount,
>;

type OriginConverter = (
	pallet_xcm::XcmPassthrough<RuntimeOrigin>,
	SignedAccountId32AsNative<AnyNetwork, RuntimeOrigin>,
);
type Barrier = AllowUnpaidExecutionFrom<Everything>;

#[derive(Clone)]
pub struct DummyWeightTrader;
impl WeightTrader for DummyWeightTrader {
	fn new() -> Self {
		DummyWeightTrader
	}

	fn buy_weight(
		&mut self,
		_weight: Weight,
		_payment: xcm_executor::AssetsInHolding,
		_context: &XcmContext,
	) -> Result<xcm_executor::AssetsInHolding, (xcm_executor::AssetsInHolding, XcmError)> {
		// Consume all payment, no refund
		Ok(xcm_executor::AssetsInHolding::new())
	}
}

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = TestMessageSender;
	type XcmEventEmitter = XcmPallet;
	type AssetTransactor = LocalAssetsTransactor;
	type OriginConverter = OriginConverter;
	type IsReserve = ();
	type IsTeleporter = ();
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type Trader = DummyWeightTrader;
	type ResponseHandler = XcmPallet;
	type AssetTrap = XcmPallet;
	type AssetLocker = ();
	type AssetExchanger = ();
	type SubscriptionService = XcmPallet;
	type PalletInstancesInfo = ();
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type FeeManager = ();
	type MessageExporter = ();
	type UniversalAliases = Nothing;
	type CallDispatcher = RuntimeCall;
	type SafeCallFilter = Everything;
	type Aliasers = Nothing;
	type TransactionalProcessor = ();
	type HrmpNewChannelOpenRequestHandler = ();
	type HrmpChannelAcceptedHandler = ();
	type HrmpChannelClosingHandler = ();
	type XcmRecorder = XcmPallet;
}

parameter_types! {
	pub TreasuryAccountId: AccountId = AccountId::new([42u8; 32]);
}

pub struct TreasuryToAccount;
impl ConvertLocation<AccountId> for TreasuryToAccount {
	fn convert_location(location: &Location) -> Option<AccountId> {
		match location.unpack() {
			(1, [Parachain(42), Plurality { id: BodyId::Treasury, part: BodyPart::Voice }]) => {
				Some(TreasuryAccountId::get())
			}, // Hardcoded test treasury account id
			_ => None,
		}
	}
}

pub(crate) type SovereignAccountOf = (
	AccountId32Aliases<AnyNetwork, AccountId>,
	TreasuryToAccount,
	HashedDescription<AccountId, DescribeFamily<DescribeAllTerminal>>,
);

impl pallet_xcm::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmRouter = TestMessageSender;
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	type XcmExecuteFilter = Everything;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Everything;
	type Weigher = FixedWeightBounds<BaseXcmWeight, RuntimeCall, MaxInstructions>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type TrustedLockers = ();
	type SovereignAccountOf = SovereignAccountOf;
	type Currency = Balances;
	type CurrencyMatcher = IsConcrete<RelayLocation>;
	type MaxLockers = frame_support::traits::ConstU32<8>;
	type MaxRemoteLockConsumers = frame_support::traits::ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
	type WeightInfo = pallet_xcm::TestWeightInfo;
	type AdminOrigin = EnsureRoot<AccountId>;
	type AuthorizedAliasConsideration = Disabled;
}

pub const UNITS: Balance = 1_000_000_000_000;
pub const INITIAL_BALANCE: Balance = 100 * UNITS;
pub const MINIMUM_BALANCE: Balance = 1 * UNITS;

pub fn sibling_chain_account_id(para_id: u32, account: [u8; 32]) -> AccountId {
	let location: Location =
		(Parent, Parachain(para_id), Junction::AccountId32 { id: account, network: None }).into();
	SovereignAccountOf::convert_location(&location).unwrap()
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let admin_account: AccountId = AccountId::new([0u8; 32]);
	pallet_assets::GenesisConfig::<Test> {
		assets: vec![
			(0, admin_account.clone(), true, MINIMUM_BALANCE),
			(1, admin_account.clone(), true, MINIMUM_BALANCE),
			(100, admin_account.clone(), true, MINIMUM_BALANCE),
		],
		metadata: vec![
			(0, "Native token".encode(), "NTV".encode(), 12),
			(1, "Relay token".encode(), "RLY".encode(), 12),
			(100, "Test token".encode(), "TST".encode(), 12),
		],
		accounts: vec![
			(0, sibling_chain_account_id(42, [3u8; 32]), INITIAL_BALANCE),
			(1, TreasuryAccountId::get(), INITIAL_BALANCE),
			(100, TreasuryAccountId::get(), INITIAL_BALANCE),
		],
		next_asset_id: None,
		reserves: vec![],
	}
	.assimilate_storage(&mut t)
	.unwrap();
	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

pub fn next_block() {
	System::set_block_number(System::block_number() + 1);
}

pub fn run_to(block_number: BlockNumber) {
	while System::block_number() < block_number {
		next_block();
	}
}
