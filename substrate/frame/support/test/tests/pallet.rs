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

use std::collections::BTreeMap;

use frame_support::{
	assert_ok, derive_impl,
	dispatch::{DispatchClass, DispatchInfo, GetDispatchInfo, Parameter, Pays},
	dispatch_context::with_context,
	pallet_prelude::{StorageInfoTrait, ValueQuery},
	parameter_types,
	storage::{unhashed, unhashed::contains_prefixed_key},
	traits::{
		ConstU32, GetCallIndex, GetCallName, GetStorageVersion, OnFinalize, OnGenesis,
		OnInitialize, OnRuntimeUpgrade, PalletError, PalletInfoAccess, SignedTransactionBuilder,
		StorageVersion, UnfilteredDispatchable,
	},
	weights::{RuntimeDbWeight, Weight},
	OrdNoBound, PartialOrdNoBound,
};
use frame_system::offchain::{CreateSignedTransaction, CreateTransactionBase, SigningTypes};
use scale_info::{meta_type, TypeInfo};
use sp_io::{
	hashing::{blake2_128, twox_128, twox_64},
	TestExternalities,
};
use sp_runtime::{
	testing::UintAuthorityId,
	traits::{Block as BlockT, Dispatchable},
	DispatchError, ModuleError,
};

parameter_types! {
	/// Used to control if the storage version should be updated.
	storage UpdateStorageVersion: bool = false;
}

pub struct SomeType1;
impl From<SomeType1> for u64 {
	fn from(_t: SomeType1) -> Self {
		0u64
	}
}

pub struct SomeType2;
impl From<SomeType2> for u64 {
	fn from(_t: SomeType2) -> Self {
		100u64
	}
}

pub struct SomeType3;
impl From<SomeType3> for u64 {
	fn from(_t: SomeType3) -> Self {
		0u64
	}
}

pub struct SomeType4;
impl From<SomeType4> for u64 {
	fn from(_t: SomeType4) -> Self {
		0u64
	}
}

pub struct SomeType5;
impl From<SomeType5> for u64 {
	fn from(_t: SomeType5) -> Self {
		0u64
	}
}

pub struct SomeType6;
impl From<SomeType6> for u64 {
	fn from(_t: SomeType6) -> Self {
		0u64
	}
}

pub struct SomeType7;
impl From<SomeType7> for u64 {
	fn from(_t: SomeType7) -> Self {
		0u64
	}
}

pub trait SomeAssociation1 {
	type _1: Parameter + codec::MaxEncodedLen + TypeInfo;
}
impl SomeAssociation1 for u64 {
	type _1 = u64;
}

pub trait SomeAssociation2 {
	type _2: Parameter + codec::MaxEncodedLen + TypeInfo;
}
impl SomeAssociation2 for u64 {
	type _2 = u64;
}

#[frame_support::pallet]
/// Pallet documentation
// Comments should not be included in the pallet documentation
#[pallet_doc("../example-pallet-doc.md")]
#[doc = include_str!("../example-readme.md")]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_runtime::DispatchResult;

	type BalanceOf<T> = <T as Config>::Balance;

	pub(crate) const STORAGE_VERSION: StorageVersion = StorageVersion::new(10);

	#[pallet::config]
	pub trait Config: frame_system::Config
	where
		<Self as frame_system::Config>::AccountId: From<SomeType1> + SomeAssociation1,
	{
		/// Some comment
		/// Some comment
		#[deprecated = "test 2"]
		#[pallet::constant]
		type MyGetParam: Get<u32>;

		/// Some comment
		/// Some comment
		#[pallet::constant]
		type MyGetParam2: Get<u32>;

		#[pallet::constant]
		type MyGetParam3: Get<<Self::AccountId as SomeAssociation1>::_1>;

		type Balance: Parameter + Default + TypeInfo;

		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
	}

	#[pallet::extra_constants]
	impl<T: Config> Pallet<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1 + From<SomeType2>,
	{
		/// Some doc
		/// Some doc
		fn some_extra() -> T::AccountId {
			SomeType2.into()
		}

		/// Some doc
		fn some_extra_extra() -> T::AccountId {
			SomeType1.into()
		}

		/// Some doc
		#[pallet::constant_name(SomeExtraRename)]
		fn some_extra_rename() -> T::AccountId {
			SomeType1.into()
		}
	}

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T>
	where
		T::AccountId: From<SomeType2> + From<SomeType1> + SomeAssociation1,
	{
		fn on_initialize(_: BlockNumberFor<T>) -> Weight {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType2); // Test for where clause
			Self::deposit_event(Event::Something(10));
			Weight::from_parts(10, 0)
		}
		fn on_finalize(_: BlockNumberFor<T>) {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType2); // Test for where clause
			Self::deposit_event(Event::Something(20));
		}
		fn on_runtime_upgrade() -> Weight {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType2); // Test for where clause
			Self::deposit_event(Event::Something(30));
			Weight::from_parts(30, 0)
		}
		fn integrity_test() {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType2); // Test for where clause
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T>
	where
		T::AccountId: From<SomeType1> + From<SomeType3> + SomeAssociation1,
	{
		/// call foo doc comment put in metadata
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::from_parts(*foo as u64, 0))]
		#[deprecated = "test"]
		pub fn foo(
			origin: OriginFor<T>,
			#[pallet::compact] foo: u32,
			_bar: u32,
		) -> DispatchResultWithPostInfo {
			let _ = foo;
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType3); // Test for where clause
			let _ = origin;
			Self::deposit_event(Event::Something(3));
			Ok(().into())
		}

		/// call foo_storage_layer doc comment put in metadata
		#[pallet::call_index(1)]
		#[pallet::weight({1})]
		pub fn foo_storage_layer(
			_origin: OriginFor<T>,
			#[pallet::compact] foo: u32,
		) -> DispatchResultWithPostInfo {
			Self::deposit_event(Event::Something(0));
			if foo == 0 {
				Err(Error::<T>::InsufficientProposersBalance)?;
			}

			Ok(().into())
		}

		#[pallet::call_index(4)]
		#[pallet::weight({1})]
		pub fn foo_index_out_of_order(_origin: OriginFor<T>) -> DispatchResult {
			Ok(())
		}

		// Test for DispatchResult return type
		#[pallet::call_index(2)]
		#[pallet::weight({1})]
		pub fn foo_no_post_info(_origin: OriginFor<T>) -> DispatchResult {
			Ok(())
		}

		#[pallet::call_index(3)]
		#[pallet::weight({1})]
		pub fn check_for_dispatch_context(_origin: OriginFor<T>) -> DispatchResult {
			with_context::<(), _>(|_| ()).ok_or_else(|| DispatchError::Unavailable)
		}

		#[cfg(feature = "frame-feature-testing")]
		#[pallet::call_index(5)]
		#[pallet::weight({1})]
		pub fn foo_feature_test(_origin: OriginFor<T>) -> DispatchResult {
			Ok(())
		}
	}

	#[pallet::error]
	#[derive(PartialEq, Eq)]
	pub enum Error<T> {
		/// error doc comment put in metadata
		InsufficientProposersBalance,
		#[deprecated = "test"]
		NonExistentStorageValue,
		Code(u8),
		#[codec(skip)]
		Skipped(u128),
		CompactU8(#[codec(compact)] u8),
		#[cfg(feature = "frame-feature-testing")]
		FeatureTest,
	}

	#[pallet::event]
	#[pallet::generate_deposit(fn deposit_event)]

	pub enum Event<T: Config>
	where
		T::AccountId: SomeAssociation1 + From<SomeType1>,
	{
		/// event doc comment put in metadata
		Proposed(<T as frame_system::Config>::AccountId),
		#[deprecated = "test"]
		Spending(BalanceOf<T>),
		Something(u32),
		SomethingElse(<T::AccountId as SomeAssociation1>::_1),
	}

	#[pallet::storage]
	pub type ValueWhereClause<T: Config>
	where
		T::AccountId: SomeAssociation2,
	= StorageValue<_, <T::AccountId as SomeAssociation2>::_2>;

	#[pallet::storage]
	pub type Value<T> = StorageValue<Value = u32>;

	#[pallet::storage]
	#[pallet::storage_prefix = "Value2"]
	pub type RenamedValue<T> = StorageValue<Value = u64>;

	/// Test some doc
	#[pallet::type_value]
	pub fn MyDefault<T: Config>() -> u16
	where
		T::AccountId: From<SomeType7> + From<SomeType1> + SomeAssociation1,
	{
		let _ = T::AccountId::from(SomeType7); // Test where clause works
		4u16
	}

	#[pallet::storage]
	pub type Map<T: Config>
	where
		T::AccountId: From<SomeType7>,
	= StorageMap<_, Blake2_128Concat, u8, u16, ValueQuery, MyDefault<T>>;

	#[pallet::storage]
	pub type Map2<T> =
		StorageMap<Hasher = Twox64Concat, Key = u16, Value = u32, MaxValues = ConstU32<3>>;

	#[pallet::storage]
	#[allow(deprecated)]
	pub type Map3<T> =
		StorageMap<_, Blake2_128Concat, u32, u64, ResultQuery<Error<T>::NonExistentStorageValue>>;

	#[pallet::storage]
	pub type DoubleMap<T> = StorageDoubleMap<_, Blake2_128Concat, u8, Twox64Concat, u16, u32>;

	#[pallet::storage]
	pub type DoubleMap2<T> = StorageDoubleMap<
		Hasher1 = Twox64Concat,
		Key1 = u16,
		Hasher2 = Blake2_128Concat,
		Key2 = u32,
		Value = u64,
		MaxValues = ConstU32<5>,
	>;

	#[pallet::storage]
	#[allow(deprecated)]
	pub type DoubleMap3<T> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		u32,
		Twox64Concat,
		u64,
		u128,
		ResultQuery<Error<T>::NonExistentStorageValue>,
	>;

	#[pallet::storage]
	#[pallet::getter(fn nmap)]
	pub type NMap<T> = StorageNMap<_, storage::Key<Blake2_128Concat, u8>, u32>;

	#[pallet::storage]
	#[pallet::getter(fn nmap2)]
	pub type NMap2<T> = StorageNMap<
		Key = (NMapKey<Twox64Concat, u16>, NMapKey<Blake2_128Concat, u32>),
		Value = u64,
		MaxValues = ConstU32<11>,
	>;

	#[pallet::storage]
	#[pallet::getter(fn nmap3)]
	#[allow(deprecated)]
	pub type NMap3<T> = StorageNMap<
		_,
		(NMapKey<Blake2_128Concat, u8>, NMapKey<Twox64Concat, u16>),
		u128,
		ResultQuery<Error<T>::NonExistentStorageValue>,
	>;

	#[pallet::storage]
	#[pallet::getter(fn counted_nmap)]
	pub type CountedNMap<T> = CountedStorageNMap<_, storage::Key<Blake2_128Concat, u8>, u32>;

	#[pallet::storage]
	#[pallet::getter(fn counted_nmap2)]
	pub type CountedNMap2<T> = CountedStorageNMap<
		Key = (NMapKey<Twox64Concat, u16>, NMapKey<Blake2_128Concat, u32>),
		Value = u64,
		MaxValues = ConstU32<11>,
	>;

	#[pallet::storage]
	#[pallet::getter(fn counted_nmap3)]
	#[allow(deprecated)]
	pub type CountedNMap3<T> = CountedStorageNMap<
		_,
		(NMapKey<Blake2_128Concat, u8>, NMapKey<Twox64Concat, u16>),
		u128,
		ResultQuery<Error<T>::NonExistentStorageValue>,
	>;

	#[pallet::storage]
	#[pallet::getter(fn conditional_value)]
	#[cfg(feature = "frame-feature-testing")]
	pub type ConditionalValue<T> = StorageValue<_, u32>;

	#[cfg(feature = "frame-feature-testing")]
	#[pallet::storage]
	#[pallet::getter(fn conditional_map)]
	pub type ConditionalMap<T> =
		StorageMap<_, Twox64Concat, u16, u32, OptionQuery, GetDefault, ConstU32<12>>;

	#[cfg(feature = "frame-feature-testing")]
	#[pallet::storage]
	#[pallet::getter(fn conditional_double_map)]
	pub type ConditionalDoubleMap<T> =
		StorageDoubleMap<_, Blake2_128Concat, u8, Twox64Concat, u16, u32>;

	#[cfg(feature = "frame-feature-testing")]
	#[pallet::storage]
	#[pallet::getter(fn conditional_nmap)]
	pub type ConditionalNMap<T> =
		StorageNMap<_, (storage::Key<Blake2_128Concat, u8>, storage::Key<Twox64Concat, u16>), u32>;

	#[cfg(feature = "frame-feature-testing")]
	#[pallet::storage]
	#[pallet::getter(fn conditional_counted_nmap)]
	pub type ConditionalCountedNMap<T> = CountedStorageNMap<
		_,
		(storage::Key<Blake2_128Concat, u8>, storage::Key<Twox64Concat, u16>),
		u32,
	>;

	#[pallet::storage]
	#[pallet::storage_prefix = "RenamedCountedMap"]
	#[pallet::getter(fn counted_storage_map)]
	pub type SomeCountedStorageMap<T> =
		CountedStorageMap<Hasher = Twox64Concat, Key = u8, Value = u32>;

	#[pallet::storage]
	#[pallet::unbounded]
	pub type Unbounded<T> = StorageValue<Value = Vec<u8>>;

	#[pallet::genesis_config]
	#[derive(frame_support::DefaultNoBound)]
	pub struct GenesisConfig<T: Config>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1 + From<SomeType4>,
	{
		#[serde(skip)]
		_config: core::marker::PhantomData<T>,
		_myfield: u32,
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1,
	{
		/// Query value no args.
		pub fn get_value() -> Option<u32> {
			Value::<T>::get()
		}

		/// Query value with args.
		pub fn get_value_with_arg(key: u16) -> Option<u32> {
			Map2::<T>::get(key)
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1 + From<SomeType4>,
	{
		fn build(&self) {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType4); // Test for where clause
		}
	}

	#[pallet::origin]
	#[derive(
		EqNoBound,
		DebugNoBound,
		CloneNoBound,
		PartialEqNoBound,
		PartialOrdNoBound,
		OrdNoBound,
		Encode,
		Decode,
		DecodeWithMemTracking,
		TypeInfo,
		MaxEncodedLen,
	)]
	pub struct Origin<T>(PhantomData<T>);

	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1 + From<SomeType5> + From<SomeType3>,
	{
		type Call = Call<T>;
		fn validate_unsigned(_source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType5); // Test for where clause
			if matches!(call, Call::foo_storage_layer { .. }) {
				return Ok(ValidTransaction::default());
			}
			Err(TransactionValidityError::Invalid(InvalidTransaction::Call))
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1 + From<SomeType6> + From<SomeType3>,
	{
		type Call = Call<T>;
		type Error = InherentError;

		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn create_inherent(_data: &InherentData) -> Option<Self::Call> {
			let _ = T::AccountId::from(SomeType1); // Test for where clause
			let _ = T::AccountId::from(SomeType6); // Test for where clause
			Some(Call::foo_no_post_info {})
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::foo_no_post_info {} | Call::foo { .. })
		}

		fn check_inherent(call: &Self::Call, _: &InherentData) -> Result<(), Self::Error> {
			match call {
				Call::foo_no_post_info {} => Ok(()),
				Call::foo { foo: 0, bar: 0 } => Err(InherentError::Fatal),
				Call::foo { .. } => Ok(()),
				_ => unreachable!("other calls are not inherents"),
			}
		}

		fn is_inherent_required(d: &InherentData) -> Result<Option<Self::Error>, Self::Error> {
			match d.get_data::<bool>(b"required") {
				Ok(Some(true)) => Ok(Some(InherentError::Fatal)),
				Ok(Some(false)) | Ok(None) => Ok(None),
				Err(_) => unreachable!("should not happen in tests"),
			}
		}
	}

	#[pallet::composite_enum]
	pub enum HoldReason {
		Staking,
	}

	#[derive(codec::Encode, Debug)]
	#[cfg_attr(feature = "std", derive(codec::Decode))]
	pub enum InherentError {
		Fatal,
	}

	impl frame_support::inherent::IsFatalError for InherentError {
		fn is_fatal_error(&self) -> bool {
			matches!(self, InherentError::Fatal)
		}
	}

	pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"testpall";
}

// Test that a pallet with non generic event and generic genesis_config is correctly handled
// and that a pallet with the attribute without_storage_info is correctly handled.
#[frame_support::pallet]
pub mod pallet2 {
	use super::{SomeAssociation1, SomeType1, UpdateStorageVersion};
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	pub(crate) const STORAGE_VERSION: StorageVersion = StorageVersion::new(2);

	#[pallet::config]
	pub trait Config: frame_system::Config
	where
		<Self as frame_system::Config>::AccountId: From<SomeType1> + SomeAssociation1,
	{
		#[allow(deprecated)]
		type RuntimeEvent: From<Event> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
	}

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1,
	{
		fn on_initialize(_: BlockNumberFor<T>) -> Weight {
			Self::deposit_event(Event::Something(11));
			Weight::zero()
		}
		fn on_finalize(_: BlockNumberFor<T>) {
			Self::deposit_event(Event::Something(21));
		}
		fn on_runtime_upgrade() -> Weight {
			Self::deposit_event(Event::Something(31));

			if UpdateStorageVersion::get() {
				Self::in_code_storage_version().put::<Self>();
			}

			Weight::zero()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> where T::AccountId: From<SomeType1> + SomeAssociation1 {}

	#[pallet::storage]
	pub type SomeValue<T: Config> = StorageValue<_, Vec<u32>>;

	#[pallet::storage]
	pub type SomeCountedStorageMap<T> =
		CountedStorageMap<Hasher = Twox64Concat, Key = u8, Value = u32>;

	#[pallet::event]
	#[pallet::generate_deposit(fn deposit_event)]
	pub enum Event {
		/// Something
		Something(u32),
	}

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1,
	{
		phantom: PhantomData<T>,
	}

	impl<T: Config> Default for GenesisConfig<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1,
	{
		fn default() -> Self {
			GenesisConfig { phantom: Default::default() }
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T>
	where
		T::AccountId: From<SomeType1> + SomeAssociation1,
	{
		fn build(&self) {}
	}

	#[pallet::composite_enum]
	pub enum HoldReason {
		Governance,
	}

	#[pallet::composite_enum]
	pub enum SlashReason {
		Equivocation,
	}
}

/// Test that the supertrait check works when we pass some parameter to the `frame_system::Config`.
#[frame_support::pallet]
pub mod pallet3 {
	#[pallet::config]
	pub trait Config:
		frame_system::Config<RuntimeOrigin = <Self as Config>::RuntimeOrigin>
	{
		type RuntimeOrigin;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);
}

#[frame_support::pallet]
pub mod pallet4 {
	#[pallet::config]
	pub trait Config: frame_system::Config {}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

/// Test that the supertrait check works when we pass some parameter to the `frame_system::Config`.
#[frame_support::pallet]
pub mod pallet5 {
	#[pallet::config]
	pub trait Config:
		frame_system::Config<RuntimeOrigin = <Self as Config>::RuntimeOrigin>
	{
		type RuntimeOrigin;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);
}

frame_support::parameter_types!(
	pub const MyGetParam3: u32 = 12;
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type BaseCallFilter = frame_support::traits::Everything;
	type RuntimeOrigin = RuntimeOrigin;
	type Nonce = u64;
	type RuntimeCall = RuntimeCall;
	type Hash = sp_runtime::testing::H256;
	type Hashing = sp_runtime::traits::BlakeTwo256;
	type AccountId = u64;
	type Lookup = sp_runtime::traits::IdentityLookup<Self::AccountId>;
	type Block = Block;
	type RuntimeEvent = RuntimeEvent;
	type BlockWeights = ();
	type BlockLength = ();
	type DbWeight = ();
	type Version = ();
	type PalletInfo = PalletInfo;
	type AccountData = ();
	type OnNewAccount = ();
	type OnKilledAccount = ();
	type SystemWeightInfo = ();
	type SS58Prefix = ();
	type OnSetCode = ();
	type MaxConsumers = ConstU32<16>;
}
impl pallet::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type MyGetParam = ConstU32<10>;
	type MyGetParam2 = ConstU32<11>;
	type MyGetParam3 = MyGetParam3;
	type Balance = u64;
}

impl pallet2::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
}

impl pallet4::Config for Runtime {}

#[cfg(feature = "frame-feature-testing")]
impl pallet3::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
}

#[cfg(feature = "frame-feature-testing-2")]
impl pallet5::Config for Runtime {
	type RuntimeOrigin = RuntimeOrigin;
}

pub type Header = sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>;
pub type Block = sp_runtime::generic::Block<Header, UncheckedExtrinsic>;
pub type UncheckedExtrinsic = sp_runtime::generic::UncheckedExtrinsic<
	u64,
	RuntimeCall,
	UintAuthorityId,
	frame_system::CheckNonZeroSender<Runtime>,
>;
pub type UncheckedSignaturePayload = sp_runtime::generic::UncheckedSignaturePayload<
	u64,
	UintAuthorityId,
	frame_system::CheckNonZeroSender<Runtime>,
>;

impl SigningTypes for Runtime {
	type Public = UintAuthorityId;
	type Signature = UintAuthorityId;
}

impl<LocalCall> CreateTransactionBase<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	type RuntimeCall = RuntimeCall;
	type Extrinsic = UncheckedExtrinsic;
}

impl<LocalCall> CreateSignedTransaction<LocalCall> for Runtime
where
	RuntimeCall: From<LocalCall>,
{
	fn create_signed_transaction<
		C: frame_system::offchain::AppCrypto<Self::Public, Self::Signature>,
	>(
		call: RuntimeCall,
		_public: UintAuthorityId,
		account: u64,
		nonce: u64,
	) -> Option<UncheckedExtrinsic> {
		Some(UncheckedExtrinsic::new_signed(
			call,
			nonce,
			account.into(),
			frame_system::CheckNonZeroSender::new(),
		))
	}
}

#[frame_support::runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeFreezeReason,
		RuntimeHoldReason,
		RuntimeSlashReason,
		RuntimeLockId,
		RuntimeTask,
		RuntimeViewFunction
	)]
	pub struct Runtime;

	#[runtime::pallet_index(0)]
	pub type System = frame_system + Call + Event<T>;

	#[runtime::pallet_index(1)]
	pub type Example = pallet;

	#[runtime::pallet_index(2)]
	#[runtime::disable_call]
	pub type Example2 = pallet2;

	#[cfg(feature = "frame-feature-testing")]
	#[runtime::pallet_index(3)]
	pub type Example3 = pallet3;

	#[runtime::pallet_index(4)]
	pub type Example4 = pallet4;

	#[cfg(feature = "frame-feature-testing-2")]
	#[runtime::pallet_index(5)]
	pub type Example5 = pallet5;
}

// Test that the part `RuntimeCall` is excluded from Example2 and included in Example4.
fn _ensure_call_is_correctly_excluded_and_included(call: RuntimeCall) {
	match call {
		RuntimeCall::System(_) | RuntimeCall::Example(_) | RuntimeCall::Example4(_) => (),
	}
}

fn maybe_docs(doc: Vec<&'static str>) -> Vec<&'static str> {
	if cfg!(feature = "no-metadata-docs") {
		vec![]
	} else {
		doc
	}
}

#[test]
#[allow(deprecated)]
fn transactional_works() {
	TestExternalities::default().execute_with(|| {
		frame_system::Pallet::<Runtime>::set_block_number(1);

		pallet::Call::<Runtime>::foo_storage_layer { foo: 0 }
			.dispatch_bypass_filter(None.into())
			.err()
			.unwrap();
		assert!(frame_system::Pallet::<Runtime>::events().is_empty());

		pallet::Call::<Runtime>::foo_storage_layer { foo: 1 }
			.dispatch_bypass_filter(None.into())
			.unwrap();
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()
				.iter()
				.map(|e| &e.event)
				.collect::<Vec<_>>(),
			vec![&RuntimeEvent::Example(pallet::Event::Something(0))],
		);
	})
}

#[test]
fn call_expand() {
	let call_foo = pallet::Call::<Runtime>::foo { foo: 3, bar: 0 };
	assert_eq!(
		call_foo.get_dispatch_info(),
		DispatchInfo {
			call_weight: frame_support::weights::Weight::from_parts(3, 0),
			extension_weight: Default::default(),
			class: DispatchClass::Normal,
			pays_fee: Pays::Yes
		}
	);
	assert_eq!(call_foo.get_call_name(), "foo");
	#[cfg(not(feature = "frame-feature-testing"))]
	assert_eq!(
		pallet::Call::<Runtime>::get_call_names(),
		&[
			"foo",
			"foo_storage_layer",
			"foo_index_out_of_order",
			"foo_no_post_info",
			"check_for_dispatch_context"
		],
	);
	#[cfg(feature = "frame-feature-testing")]
	assert_eq!(
		pallet::Call::<Runtime>::get_call_names(),
		&[
			"foo",
			"foo_storage_layer",
			"foo_index_out_of_order",
			"foo_no_post_info",
			"check_for_dispatch_context",
			"foo_feature_test"
		],
	);

	assert_eq!(call_foo.get_call_index(), 0u8);
	#[cfg(not(feature = "frame-feature-testing"))]
	assert_eq!(pallet::Call::<Runtime>::get_call_indices(), &[0u8, 1u8, 4u8, 2u8, 3u8]);
	#[cfg(feature = "frame-feature-testing")]
	assert_eq!(pallet::Call::<Runtime>::get_call_indices(), &[0u8, 1u8, 4u8, 2u8, 3u8, 5u8]);
}

#[test]
fn call_expand_index() {
	let call_foo = pallet::Call::<Runtime>::foo_index_out_of_order {};

	assert_eq!(call_foo.get_call_index(), 4u8);
	#[cfg(not(feature = "frame-feature-testing"))]
	assert_eq!(pallet::Call::<Runtime>::get_call_indices(), &[0u8, 1u8, 4u8, 2u8, 3u8]);
	#[cfg(feature = "frame-feature-testing")]
	assert_eq!(pallet::Call::<Runtime>::get_call_indices(), &[0u8, 1u8, 4u8, 2u8, 3u8, 5u8]);
}

#[test]
fn error_expand() {
	assert_eq!(
		format!("{:?}", pallet::Error::<Runtime>::InsufficientProposersBalance),
		String::from("InsufficientProposersBalance"),
	);
	assert_eq!(
		<&'static str>::from(pallet::Error::<Runtime>::InsufficientProposersBalance),
		"InsufficientProposersBalance",
	);
	assert_eq!(
		DispatchError::from(pallet::Error::<Runtime>::InsufficientProposersBalance),
		DispatchError::Module(ModuleError {
			index: 1,
			error: [0, 0, 0, 0],
			message: Some("InsufficientProposersBalance")
		}),
	);
	assert_eq!(<pallet::Error::<Runtime> as PalletError>::MAX_ENCODED_SIZE, 3);
	#[cfg(feature = "frame-feature-testing")]
	assert_eq!(format!("{:?}", pallet::Error::<Runtime>::FeatureTest), String::from("FeatureTest"),);
}

#[test]
fn instance_expand() {
	// Assert same type.
	let _: pallet::__InherentHiddenInstance = ();
}

#[test]
fn inherent_expand() {
	use frame_support::inherent::InherentData;
	use sp_core::Hasher;
	use sp_runtime::{
		traits::{BlakeTwo256, Block as _, Header},
		Digest,
	};

	let inherents = InherentData::new().create_extrinsics();

	let expected =
		vec![UncheckedExtrinsic::new_bare(RuntimeCall::Example(pallet::Call::foo_no_post_info {}))];
	assert_eq!(expected, inherents);

	let block = Block::new(
		Header::new(
			1,
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			Digest::default(),
		),
		vec![
			UncheckedExtrinsic::new_bare(RuntimeCall::Example(pallet::Call::foo_no_post_info {})),
			UncheckedExtrinsic::new_bare(RuntimeCall::Example(pallet::Call::foo {
				foo: 1,
				bar: 0,
			})),
		],
	);

	assert!(InherentData::new().check_extrinsics(&block.into()).ok());

	let block = Block::new(
		Header::new(
			1,
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			Digest::default(),
		),
		vec![
			UncheckedExtrinsic::new_bare(RuntimeCall::Example(pallet::Call::foo_no_post_info {})),
			UncheckedExtrinsic::new_bare(RuntimeCall::Example(pallet::Call::foo {
				foo: 0,
				bar: 0,
			})),
		],
	);

	assert!(InherentData::new().check_extrinsics(&block.into()).fatal_error());

	let block = Block::new(
		Header::new(
			1,
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			Digest::default(),
		),
		vec![UncheckedExtrinsic::new_bare(RuntimeCall::Example(pallet::Call::foo_storage_layer {
			foo: 0,
		}))],
	);

	let mut inherent = InherentData::new();
	inherent.put_data(*b"required", &true).unwrap();
	assert!(inherent.check_extrinsics(&block.into()).fatal_error());

	let block = Block::new(
		Header::new(
			1,
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			BlakeTwo256::hash(b"test"),
			Digest::default(),
		),
		vec![UncheckedExtrinsic::new_signed(
			RuntimeCall::Example(pallet::Call::foo_no_post_info {}),
			1,
			1.into(),
			Default::default(),
		)],
	);

	let mut inherent = InherentData::new();
	inherent.put_data(*b"required", &true).unwrap();
	assert!(inherent.check_extrinsics(&block.into()).fatal_error());
}

#[test]
#[allow(deprecated)]
fn validate_unsigned_expand() {
	use frame_support::pallet_prelude::{
		InvalidTransaction, TransactionSource, TransactionValidityError, ValidTransaction,
		ValidateUnsigned,
	};

	let call = pallet::Call::<Runtime>::foo_no_post_info {};

	let validity = pallet::Pallet::validate_unsigned(TransactionSource::Local, &call).unwrap_err();
	assert_eq!(validity, TransactionValidityError::Invalid(InvalidTransaction::Call));

	let call = pallet::Call::<Runtime>::foo_storage_layer { foo: 0 };

	let validity = pallet::Pallet::validate_unsigned(TransactionSource::External, &call).unwrap();
	assert_eq!(validity, ValidTransaction::default());
}

#[test]
fn composite_expand() {
	use codec::Encode;

	let hold_reason: RuntimeHoldReason = pallet::HoldReason::Staking.into();
	let hold_reason2: RuntimeHoldReason = pallet2::HoldReason::Governance.into();
	let slash_reason: RuntimeSlashReason = pallet2::SlashReason::Equivocation.into();

	assert_eq!(hold_reason, RuntimeHoldReason::Example(pallet::HoldReason::Staking));
	assert_eq!(hold_reason2, RuntimeHoldReason::Example2(pallet2::HoldReason::Governance));
	assert_eq!(slash_reason, RuntimeSlashReason::Example2(pallet2::SlashReason::Equivocation));

	assert_eq!(hold_reason.encode(), [1, 0]);
	assert_eq!(hold_reason2.encode(), [2, 0]);
	assert_eq!(slash_reason.encode(), [2, 0]);
}

#[test]
#[allow(deprecated)]
fn pallet_expand_deposit_event() {
	TestExternalities::default().execute_with(|| {
		frame_system::Pallet::<Runtime>::set_block_number(1);
		pallet::Call::<Runtime>::foo { foo: 3, bar: 0 }
			.dispatch_bypass_filter(None.into())
			.unwrap();
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[0].event,
			RuntimeEvent::Example(pallet::Event::Something(3)),
		);
	})
}

#[test]
fn pallet_new_call_variant() {
	pallet::Call::<Runtime>::new_call_variant_foo(3, 4);
}

#[test]
#[allow(deprecated)]
fn storage_expand() {
	use frame_support::{pallet_prelude::*, storage::StoragePrefixedMap};

	fn twox_64_concat(d: &[u8]) -> Vec<u8> {
		let mut v = twox_64(d).to_vec();
		v.extend_from_slice(d);
		v
	}

	fn blake2_128_concat(d: &[u8]) -> Vec<u8> {
		let mut v = blake2_128(d).to_vec();
		v.extend_from_slice(d);
		v
	}

	TestExternalities::default().execute_with(|| {
		pallet::Value::<Runtime>::put(1);
		let k = [twox_128(b"Example"), twox_128(b"Value")].concat();
		assert_eq!(unhashed::get::<u32>(&k), Some(1u32));

		pallet::RenamedValue::<Runtime>::put(2);
		let k = [twox_128(b"Example"), twox_128(b"Value2")].concat();
		assert_eq!(unhashed::get::<u64>(&k), Some(2));

		pallet::Map::<Runtime>::insert(1, 2);
		let mut k = [twox_128(b"Example"), twox_128(b"Map")].concat();
		k.extend(1u8.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u16>(&k), Some(2u16));
		assert_eq!(&k[..32], &<pallet::Map<Runtime>>::final_prefix());

		pallet::Map2::<Runtime>::insert(1, 2);
		let mut k = [twox_128(b"Example"), twox_128(b"Map2")].concat();
		k.extend(1u16.using_encoded(twox_64_concat));
		assert_eq!(unhashed::get::<u32>(&k), Some(2u32));
		assert_eq!(&k[..32], &<pallet::Map2<Runtime>>::final_prefix());

		pallet::Map3::<Runtime>::insert(1, 2);
		let mut k = [twox_128(b"Example"), twox_128(b"Map3")].concat();
		k.extend(1u32.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u64>(&k), Some(2u64));
		assert_eq!(&k[..32], &<pallet::Map3<Runtime>>::final_prefix());
		assert_eq!(
			pallet::Map3::<Runtime>::get(2),
			Err(pallet::Error::<Runtime>::NonExistentStorageValue),
		);

		pallet::DoubleMap::<Runtime>::insert(&1, &2, &3);
		let mut k = [twox_128(b"Example"), twox_128(b"DoubleMap")].concat();
		k.extend(1u8.using_encoded(blake2_128_concat));
		k.extend(2u16.using_encoded(twox_64_concat));
		assert_eq!(unhashed::get::<u32>(&k), Some(3u32));
		assert_eq!(&k[..32], &<pallet::DoubleMap<Runtime>>::final_prefix());

		pallet::DoubleMap2::<Runtime>::insert(&1, &2, &3);
		let mut k = [twox_128(b"Example"), twox_128(b"DoubleMap2")].concat();
		k.extend(1u16.using_encoded(twox_64_concat));
		k.extend(2u32.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u64>(&k), Some(3u64));
		assert_eq!(&k[..32], &<pallet::DoubleMap2<Runtime>>::final_prefix());

		pallet::DoubleMap3::<Runtime>::insert(&1, &2, &3);
		let mut k = [twox_128(b"Example"), twox_128(b"DoubleMap3")].concat();
		k.extend(1u32.using_encoded(blake2_128_concat));
		k.extend(2u64.using_encoded(twox_64_concat));
		assert_eq!(unhashed::get::<u128>(&k), Some(3u128));
		assert_eq!(&k[..32], &<pallet::DoubleMap3<Runtime>>::final_prefix());
		assert_eq!(
			pallet::DoubleMap3::<Runtime>::get(2, 3),
			Err(pallet::Error::<Runtime>::NonExistentStorageValue),
		);

		pallet::NMap::<Runtime>::insert((&1,), &3);
		let mut k = [twox_128(b"Example"), twox_128(b"NMap")].concat();
		k.extend(1u8.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u32>(&k), Some(3u32));
		assert_eq!(&k[..32], &<pallet::NMap<Runtime>>::final_prefix());

		pallet::NMap2::<Runtime>::insert((&1, &2), &3);
		let mut k = [twox_128(b"Example"), twox_128(b"NMap2")].concat();
		k.extend(1u16.using_encoded(twox_64_concat));
		k.extend(2u32.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u64>(&k), Some(3u64));
		assert_eq!(&k[..32], &<pallet::NMap2<Runtime>>::final_prefix());
		assert_eq!(pallet::Pallet::<Runtime>::nmap2((1, 2)), Some(3u64));

		pallet::NMap3::<Runtime>::insert((&1, &2), &3);
		let mut k = [twox_128(b"Example"), twox_128(b"NMap3")].concat();
		k.extend(1u8.using_encoded(blake2_128_concat));
		k.extend(2u16.using_encoded(twox_64_concat));
		assert_eq!(unhashed::get::<u128>(&k), Some(3u128));
		assert_eq!(&k[..32], &<pallet::NMap3<Runtime>>::final_prefix());
		assert_eq!(pallet::Pallet::<Runtime>::nmap3((1, 2)), Ok(3u128));
		assert_eq!(
			pallet::NMap3::<Runtime>::get((2, 3)),
			Err(pallet::Error::<Runtime>::NonExistentStorageValue),
		);

		pallet::CountedNMap::<Runtime>::insert((&1,), &3);
		let mut k = [twox_128(b"Example"), twox_128(b"CountedNMap")].concat();
		k.extend(1u8.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u32>(&k), Some(3u32));
		assert_eq!(pallet::CountedNMap::<Runtime>::count(), 1);
		assert_eq!(
			unhashed::get::<u32>(
				&[twox_128(b"Example"), twox_128(b"CounterForCountedNMap")].concat()
			),
			Some(1u32)
		);

		pallet::CountedNMap2::<Runtime>::insert((&1, &2), &3);
		let mut k = [twox_128(b"Example"), twox_128(b"CountedNMap2")].concat();
		k.extend(1u16.using_encoded(twox_64_concat));
		k.extend(2u32.using_encoded(blake2_128_concat));
		assert_eq!(unhashed::get::<u64>(&k), Some(3u64));
		assert_eq!(pallet::CountedNMap2::<Runtime>::count(), 1);
		assert_eq!(
			unhashed::get::<u32>(
				&[twox_128(b"Example"), twox_128(b"CounterForCountedNMap2")].concat()
			),
			Some(1u32)
		);
		assert_eq!(pallet::Pallet::<Runtime>::counted_nmap2((1, 2)), Some(3u64));

		pallet::CountedNMap3::<Runtime>::insert((&1, &2), &3);
		let mut k = [twox_128(b"Example"), twox_128(b"CountedNMap3")].concat();
		k.extend(1u8.using_encoded(blake2_128_concat));
		k.extend(2u16.using_encoded(twox_64_concat));
		assert_eq!(pallet::CountedNMap3::<Runtime>::count(), 1);
		assert_eq!(unhashed::get::<u128>(&k), Some(3u128));
		assert_eq!(pallet::Pallet::<Runtime>::counted_nmap3((1, 2)), Ok(3u128));
		assert_eq!(
			pallet::CountedNMap3::<Runtime>::get((2, 3)),
			Err(pallet::Error::<Runtime>::NonExistentStorageValue),
		);
		assert_eq!(
			unhashed::get::<u32>(
				&[twox_128(b"Example"), twox_128(b"CounterForCountedNMap3")].concat()
			),
			Some(1u32)
		);

		#[cfg(feature = "frame-feature-testing")]
		{
			pallet::ConditionalValue::<Runtime>::put(1);
			pallet::ConditionalMap::<Runtime>::insert(1, 2);
			pallet::ConditionalDoubleMap::<Runtime>::insert(1, 2, 3);
			pallet::ConditionalNMap::<Runtime>::insert((1, 2), 3);
		}

		pallet::SomeCountedStorageMap::<Runtime>::insert(1, 2);
		let mut k = [twox_128(b"Example"), twox_128(b"RenamedCountedMap")].concat();
		k.extend(1u8.using_encoded(twox_64_concat));
		assert_eq!(unhashed::get::<u32>(&k), Some(2u32));
		let k = [twox_128(b"Example"), twox_128(b"CounterForRenamedCountedMap")].concat();
		assert_eq!(unhashed::get::<u32>(&k), Some(1u32));

		pallet::Unbounded::<Runtime>::put(vec![1, 2]);
		let k = [twox_128(b"Example"), twox_128(b"Unbounded")].concat();
		assert_eq!(unhashed::get::<Vec<u8>>(&k), Some(vec![1, 2]));
	})
}

#[test]
#[allow(deprecated)]
fn pallet_hooks_expand() {
	TestExternalities::default().execute_with(|| {
		frame_system::Pallet::<Runtime>::set_block_number(1);

		assert_eq!(AllPalletsWithoutSystem::on_initialize(1), Weight::from_parts(10, 0));
		AllPalletsWithoutSystem::on_finalize(1);

		assert_eq!(AllPalletsWithoutSystem::on_runtime_upgrade(), Weight::from_parts(30, 0));

		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[0].event,
			RuntimeEvent::Example(pallet::Event::Something(10)),
		);
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[1].event,
			RuntimeEvent::Example2(pallet2::Event::Something(11)),
		);
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[2].event,
			RuntimeEvent::Example(pallet::Event::Something(20)),
		);
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[3].event,
			RuntimeEvent::Example2(pallet2::Event::Something(21)),
		);
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[4].event,
			RuntimeEvent::Example(pallet::Event::Something(30)),
		);
		assert_eq!(
			frame_system::Pallet::<Runtime>::events()[5].event,
			RuntimeEvent::Example2(pallet2::Event::Something(31)),
		);
	})
}

#[test]
fn pallet_on_genesis() {
	TestExternalities::default().execute_with(|| {
		assert_eq!(pallet::Pallet::<Runtime>::on_chain_storage_version(), StorageVersion::new(0));
		pallet::Pallet::<Runtime>::on_genesis();
		assert_eq!(
			pallet::Pallet::<Runtime>::in_code_storage_version(),
			pallet::Pallet::<Runtime>::on_chain_storage_version(),
		);
	})
}

#[test]
fn migrate_from_pallet_version_to_storage_version() {
	const PALLET_VERSION_STORAGE_KEY_POSTFIX: &[u8] = b":__PALLET_VERSION__:";

	fn pallet_version_key(name: &str) -> [u8; 32] {
		frame_support::storage::storage_prefix(name.as_bytes(), PALLET_VERSION_STORAGE_KEY_POSTFIX)
	}

	TestExternalities::default().execute_with(|| {
		// Insert some fake pallet versions
		sp_io::storage::set(&pallet_version_key(Example::name()), &[1, 2, 3]);
		sp_io::storage::set(&pallet_version_key(Example2::name()), &[1, 2, 3]);
		sp_io::storage::set(&pallet_version_key(System::name()), &[1, 2, 3]);

		// Check that everyone currently is at version 0
		assert_eq!(Example::on_chain_storage_version(), StorageVersion::new(0));
		assert_eq!(Example2::on_chain_storage_version(), StorageVersion::new(0));
		assert_eq!(System::on_chain_storage_version(), StorageVersion::new(0));

		let db_weight = RuntimeDbWeight { read: 0, write: 5 };
		let weight = frame_support::migrations::migrate_from_pallet_version_to_storage_version::<
			AllPalletsWithSystem,
		>(&db_weight);

		let mut pallet_num = 4;
		if cfg!(feature = "frame-feature-testing") {
			pallet_num += 1;
		};
		if cfg!(feature = "frame-feature-testing-2") {
			pallet_num += 1;
		};

		// `pallet_num` pallets, 2 writes and every write costs 5 weight.
		assert_eq!(Weight::from_parts(pallet_num * 2 * 5, 0), weight);

		// All pallet versions should be removed
		assert!(sp_io::storage::get(&pallet_version_key(Example::name())).is_none());
		assert!(sp_io::storage::get(&pallet_version_key(Example2::name())).is_none());
		assert!(sp_io::storage::get(&pallet_version_key(System::name())).is_none());

		assert_eq!(Example::on_chain_storage_version(), pallet::STORAGE_VERSION);
		assert_eq!(Example2::on_chain_storage_version(), pallet2::STORAGE_VERSION);
		assert_eq!(System::on_chain_storage_version(), StorageVersion::new(0));
	});
}

#[test]
#[allow(deprecated)]
fn pallet_item_docs_in_metadata() {
	// call
	let call_variants = match meta_type::<pallet::Call<Runtime>>().type_info().type_def {
		scale_info::TypeDef::Variant(variants) => variants.variants,
		_ => unreachable!(),
	};

	assert_eq!(call_variants[0].docs, maybe_docs(vec!["call foo doc comment put in metadata"]));
	assert_eq!(
		call_variants[1].docs,
		maybe_docs(vec!["call foo_storage_layer doc comment put in metadata"])
	);
	assert!(call_variants[2].docs.is_empty());

	// event
	let event_variants = match meta_type::<pallet::Event<Runtime>>().type_info().type_def {
		scale_info::TypeDef::Variant(variants) => variants.variants,
		_ => unreachable!(),
	};

	assert_eq!(event_variants[0].docs, maybe_docs(vec!["event doc comment put in metadata"]));
	assert!(event_variants[1].docs.is_empty());

	// error
	let error_variants = match meta_type::<pallet::Error<Runtime>>().type_info().type_def {
		scale_info::TypeDef::Variant(variants) => variants.variants,
		_ => unreachable!(),
	};

	assert_eq!(error_variants[0].docs, maybe_docs(vec!["error doc comment put in metadata"]));
	assert!(error_variants[1].docs.is_empty());

	// storage is already covered in the main `fn metadata` test.
}

#[test]
#[allow(deprecated)]
fn metadata_v15() {
	use codec::Decode;
	use frame_metadata::{v15::*, *};

	let readme = "Very important information :D\n";
	let pallet_doc = "This is the best pallet\n";
	let expected_pallet_doc = vec![" Pallet documentation", readme, pallet_doc];

	let pallets = vec![
		PalletMetadata {
			index: 1,
			name: "Example",
			storage: Some(PalletStorageMetadata {
				prefix: "Example",
				entries: vec![
					StorageEntryMetadata {
						name: "ValueWhereClause",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Plain(meta_type::<u64>()),
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "Value",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "Value2",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Plain(meta_type::<u64>()),
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "Map",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Map {
							key: meta_type::<u8>(),
							value: meta_type::<u16>(),
							hashers: vec![StorageHasher::Blake2_128Concat],
						},
						default: vec![4, 0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "Map2",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<u16>(),
							value: meta_type::<u32>(),
							hashers: vec![StorageHasher::Twox64Concat],
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "Map3",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<u32>(),
							value: meta_type::<u64>(),
							hashers: vec![StorageHasher::Blake2_128Concat],
						},
						default: vec![1, 1],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "DoubleMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							value: meta_type::<u32>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
							key: meta_type::<(u8, u16)>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "DoubleMap2",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							value: meta_type::<u64>(),
							key: meta_type::<(u16, u32)>(),
							hashers: vec![
								StorageHasher::Twox64Concat,
								StorageHasher::Blake2_128Concat,
							],
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "DoubleMap3",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							value: meta_type::<u128>(),
							key: meta_type::<(u32, u64)>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
						},
						default: vec![1, 1],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "NMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<u8>(),
							hashers: vec![StorageHasher::Blake2_128Concat],
							value: meta_type::<u32>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "NMap2",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<(u16, u32)>(),
							hashers: vec![
								StorageHasher::Twox64Concat,
								StorageHasher::Blake2_128Concat,
							],
							value: meta_type::<u64>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "NMap3",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<(u8, u16)>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
							value: meta_type::<u128>(),
						},
						default: vec![1, 1],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "CountedNMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<u8>(),
							hashers: vec![StorageHasher::Blake2_128Concat],
							value: meta_type::<u32>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "CounterForCountedNMap",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0, 0, 0, 0],
						docs: maybe_docs(vec!["Counter for the related counted storage map"]),
					},
					StorageEntryMetadata {
						name: "CountedNMap2",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<(u16, u32)>(),
							hashers: vec![
								StorageHasher::Twox64Concat,
								StorageHasher::Blake2_128Concat,
							],
							value: meta_type::<u64>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "CounterForCountedNMap2",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0, 0, 0, 0],
						docs: maybe_docs(vec!["Counter for the related counted storage map"]),
					},
					StorageEntryMetadata {
						name: "CountedNMap3",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<(u8, u16)>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
							value: meta_type::<u128>(),
						},
						default: vec![1, 1],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "CounterForCountedNMap3",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0, 0, 0, 0],
						docs: maybe_docs(vec!["Counter for the related counted storage map"]),
					},
					#[cfg(feature = "frame-feature-testing")]
					StorageEntryMetadata {
						name: "ConditionalValue",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0],
						docs: vec![],
					},
					#[cfg(feature = "frame-feature-testing")]
					StorageEntryMetadata {
						name: "ConditionalMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<u16>(),
							value: meta_type::<u32>(),
							hashers: vec![StorageHasher::Twox64Concat],
						},
						default: vec![0],
						docs: vec![],
					},
					#[cfg(feature = "frame-feature-testing")]
					StorageEntryMetadata {
						name: "ConditionalDoubleMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							value: meta_type::<u32>(),
							key: meta_type::<(u8, u16)>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
						},
						default: vec![0],
						docs: vec![],
					},
					#[cfg(feature = "frame-feature-testing")]
					StorageEntryMetadata {
						name: "ConditionalNMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<(u8, u16)>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
							value: meta_type::<u32>(),
						},
						default: vec![0],
						docs: vec![],
					},
					#[cfg(feature = "frame-feature-testing")]
					StorageEntryMetadata {
						name: "ConditionalCountedNMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							key: meta_type::<(u8, u16)>(),
							hashers: vec![
								StorageHasher::Blake2_128Concat,
								StorageHasher::Twox64Concat,
							],
							value: meta_type::<u32>(),
						},
						default: vec![0],
						docs: vec![],
					},
					#[cfg(feature = "frame-feature-testing")]
					StorageEntryMetadata {
						name: "CounterForConditionalCountedNMap",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0, 0, 0, 0],
						docs: maybe_docs(vec!["Counter for the related counted storage map"]),
					},
					StorageEntryMetadata {
						name: "RenamedCountedMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							hashers: vec![StorageHasher::Twox64Concat],
							key: meta_type::<u8>(),
							value: meta_type::<u32>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "CounterForRenamedCountedMap",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0, 0, 0, 0],
						docs: maybe_docs(vec!["Counter for the related counted storage map"]),
					},
					StorageEntryMetadata {
						name: "Unbounded",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Plain(meta_type::<Vec<u8>>()),
						default: vec![0],
						docs: vec![],
					},
				],
			}),
			calls: Some(meta_type::<pallet::Call<Runtime>>().into()),
			event: Some(meta_type::<pallet::Event<Runtime>>().into()),
			constants: vec![
				PalletConstantMetadata {
					name: "MyGetParam",
					ty: meta_type::<u32>(),
					value: vec![10, 0, 0, 0],
					docs: maybe_docs(vec![" Some comment", " Some comment"]),
				},
				PalletConstantMetadata {
					name: "MyGetParam2",
					ty: meta_type::<u32>(),
					value: vec![11, 0, 0, 0],
					docs: maybe_docs(vec![" Some comment", " Some comment"]),
				},
				PalletConstantMetadata {
					name: "MyGetParam3",
					ty: meta_type::<u64>(),
					value: vec![12, 0, 0, 0, 0, 0, 0, 0],
					docs: vec![],
				},
				PalletConstantMetadata {
					name: "some_extra",
					ty: meta_type::<u64>(),
					value: vec![100, 0, 0, 0, 0, 0, 0, 0],
					docs: maybe_docs(vec![" Some doc", " Some doc"]),
				},
				PalletConstantMetadata {
					name: "some_extra_extra",
					ty: meta_type::<u64>(),
					value: vec![0, 0, 0, 0, 0, 0, 0, 0],
					docs: maybe_docs(vec![" Some doc"]),
				},
				PalletConstantMetadata {
					name: "SomeExtraRename",
					ty: meta_type::<u64>(),
					value: vec![0, 0, 0, 0, 0, 0, 0, 0],
					docs: maybe_docs(vec![" Some doc"]),
				},
			],
			error: Some(PalletErrorMetadata { ty: meta_type::<pallet::Error<Runtime>>() }),
			docs: expected_pallet_doc,
		},
		PalletMetadata {
			index: 2,
			name: "Example2",
			storage: Some(PalletStorageMetadata {
				prefix: "Example2",
				entries: vec![
					StorageEntryMetadata {
						name: "SomeValue",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Plain(meta_type::<Vec<u32>>()),
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "SomeCountedStorageMap",
						modifier: StorageEntryModifier::Optional,
						ty: StorageEntryType::Map {
							hashers: vec![StorageHasher::Twox64Concat],
							key: meta_type::<u8>(),
							value: meta_type::<u32>(),
						},
						default: vec![0],
						docs: vec![],
					},
					StorageEntryMetadata {
						name: "CounterForSomeCountedStorageMap",
						modifier: StorageEntryModifier::Default,
						ty: StorageEntryType::Plain(meta_type::<u32>()),
						default: vec![0, 0, 0, 0],
						docs: maybe_docs(vec!["Counter for the related counted storage map"]),
					},
				],
			}),
			calls: None,
			event: Some(PalletEventMetadata { ty: meta_type::<pallet2::Event>() }),
			constants: vec![],
			error: None,
			docs: vec![],
		},
		#[cfg(feature = "frame-feature-testing")]
		PalletMetadata {
			index: 3,
			name: "Example3",
			storage: None,
			calls: None,
			event: None,
			constants: vec![],
			error: None,
			docs: vec![" Test that the supertrait check works when we pass some parameter to the `frame_system::Config`."],
		},
		PalletMetadata {
			index: 4,
			name: "Example4",
			storage: None,
			calls: Some(meta_type::<pallet4::Call<Runtime>>().into()),
			event: None,
			constants: vec![],
			error: None,
			docs: vec![],
		},
		#[cfg(feature = "frame-feature-testing-2")]
		PalletMetadata {
			index: 5,
			name: "Example5",
			storage: None,
			calls: None,
			event: None,
			constants: vec![],
			error: None,
			docs: vec![" Test that the supertrait check works when we pass some parameter to the `frame_system::Config`."],
		},
	];

	let empty_doc = pallets[0].event.as_ref().unwrap().ty.type_info().docs.is_empty() &&
		pallets[0].error.as_ref().unwrap().ty.type_info().docs.is_empty() &&
		pallets[0].calls.as_ref().unwrap().ty.type_info().docs.is_empty();

	if cfg!(feature = "no-metadata-docs") {
		assert!(empty_doc)
	} else {
		assert!(!empty_doc)
	}

	let extrinsic = ExtrinsicMetadata {
		version: 5,
		signed_extensions: vec![SignedExtensionMetadata {
			identifier: "UnitSignedExtension",
			ty: meta_type::<()>(),
			additional_signed: meta_type::<()>(),
		}],
		address_ty: meta_type::<
			<<<Runtime as frame_system::Config>::Block as BlockT>::Extrinsic as SignedTransactionBuilder>::Address
		>(),
		call_ty: meta_type::<<Runtime as CreateTransactionBase<RuntimeCall>>::RuntimeCall>(),
		signature_ty: meta_type::<
			<<<Runtime as frame_system::Config>::Block as BlockT>::Extrinsic as SignedTransactionBuilder>::Signature
		>(),
		extra_ty: meta_type::<
			<<<Runtime as frame_system::Config>::Block as BlockT>::Extrinsic as SignedTransactionBuilder>::Extension
		>(),
	};

	let outer_enums = OuterEnums {
		call_enum_ty: meta_type::<RuntimeCall>(),
		event_enum_ty: meta_type::<RuntimeEvent>(),
		error_enum_ty: meta_type::<RuntimeError>(),
	};

	let expected_metadata: RuntimeMetadataPrefixed = RuntimeMetadataLastVersion::new(
		pallets,
		extrinsic,
		meta_type::<Runtime>(),
		vec![],
		outer_enums,
		CustomMetadata { map: Default::default() },
	)
	.into();
	let expected_metadata = match expected_metadata.1 {
		RuntimeMetadata::V15(metadata) => metadata,
		_ => panic!("metadata has been bumped, test needs to be updated"),
	};

	let bytes = &Runtime::metadata_at_version(15).expect("Metadata must be present; qed");

	let actual_metadata: RuntimeMetadataPrefixed =
		Decode::decode(&mut &bytes[..]).expect("Metadata encoded properly; qed");

	let actual_metadata = match actual_metadata.1 {
		RuntimeMetadata::V15(metadata) => metadata,
		_ => panic!("metadata has been bumped, test needs to be updated"),
	};

	pretty_assertions::assert_eq!(actual_metadata.pallets, expected_metadata.pallets);
}

#[test]
fn metadata_at_version() {
	use frame_metadata::*;
	use sp_core::Decode;

	// Metadata always returns the V14.3
	let metadata = Runtime::metadata();
	let at_metadata = match Runtime::metadata_at_version(14) {
		Some(opaque) => {
			let bytes = &*opaque;
			let metadata: RuntimeMetadataPrefixed = Decode::decode(&mut &bytes[..]).unwrap();
			metadata
		},
		_ => panic!("metadata has been bumped, test needs to be updated"),
	};

	assert_eq!(metadata, at_metadata);
}

#[test]
fn metadata_versions() {
	assert_eq!(vec![14, 15, 16], Runtime::metadata_versions());
}

#[test]
fn metadata_ir_pallet_runtime_docs() {
	let ir = Runtime::metadata_ir();
	let pallet = ir
		.pallets
		.iter()
		.find(|pallet| pallet.name == "Example")
		.expect("Pallet should be present");

	let readme = "Very important information :D\n";
	let pallet_doc = "This is the best pallet\n";
	let expected = vec![" Pallet documentation", readme, pallet_doc];
	assert_eq!(pallet.docs, expected);
}

#[test]
fn extrinsic_metadata_ir_types() {
	let ir = Runtime::metadata_ir().extrinsic;

	assert_eq!(
		meta_type::<<<<Runtime as frame_system::Config>::Block as BlockT>::Extrinsic as SignedTransactionBuilder>::Address>(),
		ir.address_ty
	);
	assert_eq!(meta_type::<u64>(), ir.address_ty);

	assert_eq!(
		meta_type::<<Runtime as CreateTransactionBase<RuntimeCall>>::RuntimeCall>(),
		ir.call_ty
	);
	assert_eq!(meta_type::<RuntimeCall>(), ir.call_ty);

	assert_eq!(
		meta_type::<<<<Runtime as frame_system::Config>::Block as BlockT>::Extrinsic as SignedTransactionBuilder>::Signature>(),
		ir.signature_ty
	);
	assert_eq!(meta_type::<UintAuthorityId>(), ir.signature_ty);

	assert_eq!(
		meta_type::<<<<Runtime as frame_system::Config>::Block as BlockT>::Extrinsic as SignedTransactionBuilder>::Extension>(),
		ir.extra_ty
	);
	assert_eq!(meta_type::<frame_system::CheckNonZeroSender<Runtime>>(), ir.extra_ty);
}

#[test]
fn test_pallet_runtime_docs() {
	let docs = crate::pallet::Pallet::<Runtime>::pallet_documentation_metadata();
	let readme = "Very important information :D\n";
	let pallet_doc = "This is the best pallet\n";
	let expected = vec![" Pallet documentation", readme, pallet_doc];
	assert_eq!(docs, expected);
}

#[test]
fn test_pallet_info_access() {
	assert_eq!(<System as frame_support::traits::PalletInfoAccess>::name(), "System");
	assert_eq!(<Example as frame_support::traits::PalletInfoAccess>::name(), "Example");
	assert_eq!(<Example2 as frame_support::traits::PalletInfoAccess>::name(), "Example2");
	assert_eq!(<System as frame_support::traits::PalletInfoAccess>::index(), 0);
	assert_eq!(<Example as frame_support::traits::PalletInfoAccess>::index(), 1);
	assert_eq!(<Example2 as frame_support::traits::PalletInfoAccess>::index(), 2);
}

#[test]
fn test_storage_info() {
	use frame_support::{
		storage::storage_prefix as prefix,
		traits::{StorageInfo, StorageInfoTrait},
	};

	// Storage max size is calculated by adding up all the hasher size, the key type size and the
	// value type size
	assert_eq!(
		Example::storage_info(),
		vec![
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"ValueWhereClause".to_vec(),
				prefix: prefix(b"Example", b"ValueWhereClause").to_vec(),
				max_values: Some(1),
				max_size: Some(8),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"Value".to_vec(),
				prefix: prefix(b"Example", b"Value").to_vec(),
				max_values: Some(1),
				max_size: Some(4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"Value2".to_vec(),
				prefix: prefix(b"Example", b"Value2").to_vec(),
				max_values: Some(1),
				max_size: Some(8),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"Map".to_vec(),
				prefix: prefix(b"Example", b"Map").to_vec(),
				max_values: None,
				max_size: Some(16 + 1 + 2),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"Map2".to_vec(),
				prefix: prefix(b"Example", b"Map2").to_vec(),
				max_values: Some(3),
				max_size: Some(8 + 2 + 4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"Map3".to_vec(),
				prefix: prefix(b"Example", b"Map3").to_vec(),
				max_values: None,
				max_size: Some(16 + 4 + 8),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"DoubleMap".to_vec(),
				prefix: prefix(b"Example", b"DoubleMap").to_vec(),
				max_values: None,
				max_size: Some(16 + 1 + 8 + 2 + 4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"DoubleMap2".to_vec(),
				prefix: prefix(b"Example", b"DoubleMap2").to_vec(),
				max_values: Some(5),
				max_size: Some(8 + 2 + 16 + 4 + 8),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"DoubleMap3".to_vec(),
				prefix: prefix(b"Example", b"DoubleMap3").to_vec(),
				max_values: None,
				max_size: Some(16 + 4 + 8 + 8 + 16),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"NMap".to_vec(),
				prefix: prefix(b"Example", b"NMap").to_vec(),
				max_values: None,
				max_size: Some(16 + 1 + 4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"NMap2".to_vec(),
				prefix: prefix(b"Example", b"NMap2").to_vec(),
				max_values: Some(11),
				max_size: Some(8 + 2 + 16 + 4 + 8),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"NMap3".to_vec(),
				prefix: prefix(b"Example", b"NMap3").to_vec(),
				max_values: None,
				max_size: Some(16 + 1 + 8 + 2 + 16),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CountedNMap".to_vec(),
				prefix: prefix(b"Example", b"CountedNMap").to_vec(),
				max_values: None,
				max_size: Some(16 + 1 + 4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CounterForCountedNMap".to_vec(),
				prefix: prefix(b"Example", b"CounterForCountedNMap").to_vec(),
				max_values: Some(1),
				max_size: Some(4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CountedNMap2".to_vec(),
				prefix: prefix(b"Example", b"CountedNMap2").to_vec(),
				max_values: Some(11),
				max_size: Some(8 + 2 + 16 + 4 + 8),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CounterForCountedNMap2".to_vec(),
				prefix: prefix(b"Example", b"CounterForCountedNMap2").to_vec(),
				max_values: Some(1),
				max_size: Some(4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CountedNMap3".to_vec(),
				prefix: prefix(b"Example", b"CountedNMap3").to_vec(),
				max_values: None,
				max_size: Some(16 + 1 + 8 + 2 + 16),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CounterForCountedNMap3".to_vec(),
				prefix: prefix(b"Example", b"CounterForCountedNMap3").to_vec(),
				max_values: Some(1),
				max_size: Some(4),
			},
			#[cfg(feature = "frame-feature-testing")]
			{
				StorageInfo {
					pallet_name: b"Example".to_vec(),
					storage_name: b"ConditionalValue".to_vec(),
					prefix: prefix(b"Example", b"ConditionalValue").to_vec(),
					max_values: Some(1),
					max_size: Some(4),
				}
			},
			#[cfg(feature = "frame-feature-testing")]
			{
				StorageInfo {
					pallet_name: b"Example".to_vec(),
					storage_name: b"ConditionalMap".to_vec(),
					prefix: prefix(b"Example", b"ConditionalMap").to_vec(),
					max_values: Some(12),
					max_size: Some(8 + 2 + 4),
				}
			},
			#[cfg(feature = "frame-feature-testing")]
			{
				StorageInfo {
					pallet_name: b"Example".to_vec(),
					storage_name: b"ConditionalDoubleMap".to_vec(),
					prefix: prefix(b"Example", b"ConditionalDoubleMap").to_vec(),
					max_values: None,
					max_size: Some(16 + 1 + 8 + 2 + 4),
				}
			},
			#[cfg(feature = "frame-feature-testing")]
			{
				StorageInfo {
					pallet_name: b"Example".to_vec(),
					storage_name: b"ConditionalNMap".to_vec(),
					prefix: prefix(b"Example", b"ConditionalNMap").to_vec(),
					max_values: None,
					max_size: Some(16 + 1 + 8 + 2 + 4),
				}
			},
			#[cfg(feature = "frame-feature-testing")]
			{
				StorageInfo {
					pallet_name: b"Example".to_vec(),
					storage_name: b"ConditionalCountedNMap".to_vec(),
					prefix: prefix(b"Example", b"ConditionalCountedNMap").to_vec(),
					max_values: None,
					max_size: Some(16 + 1 + 8 + 2 + 4),
				}
			},
			#[cfg(feature = "frame-feature-testing")]
			{
				StorageInfo {
					pallet_name: b"Example".to_vec(),
					storage_name: b"CounterForConditionalCountedNMap".to_vec(),
					prefix: prefix(b"Example", b"CounterForConditionalCountedNMap").to_vec(),
					max_values: Some(1),
					max_size: Some(4),
				}
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"RenamedCountedMap".to_vec(),
				prefix: prefix(b"Example", b"RenamedCountedMap").to_vec(),
				max_values: None,
				max_size: Some(8 + 1 + 4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"CounterForRenamedCountedMap".to_vec(),
				prefix: prefix(b"Example", b"CounterForRenamedCountedMap").to_vec(),
				max_values: Some(1),
				max_size: Some(4),
			},
			StorageInfo {
				pallet_name: b"Example".to_vec(),
				storage_name: b"Unbounded".to_vec(),
				prefix: prefix(b"Example", b"Unbounded").to_vec(),
				max_values: Some(1),
				max_size: None,
			},
		],
	);

	assert_eq!(
		Example2::storage_info(),
		vec![
			StorageInfo {
				pallet_name: b"Example2".to_vec(),
				storage_name: b"SomeValue".to_vec(),
				prefix: prefix(b"Example2", b"SomeValue").to_vec(),
				max_values: Some(1),
				max_size: None,
			},
			StorageInfo {
				pallet_name: b"Example2".to_vec(),
				storage_name: b"SomeCountedStorageMap".to_vec(),
				prefix: prefix(b"Example2", b"SomeCountedStorageMap").to_vec(),
				max_values: None,
				max_size: None,
			},
			StorageInfo {
				pallet_name: b"Example2".to_vec(),
				storage_name: b"CounterForSomeCountedStorageMap".to_vec(),
				prefix: prefix(b"Example2", b"CounterForSomeCountedStorageMap").to_vec(),
				max_values: Some(1),
				max_size: Some(4),
			},
		],
	);
}

#[test]
fn assert_type_all_pallets_with_system_is_correct() {
	// Just ensure the 2 types are same.
	fn _a(_t: AllPalletsWithSystem) {}
	#[cfg(all(not(feature = "frame-feature-testing"), not(feature = "frame-feature-testing-2")))]
	fn _b(t: (System, Example, Example2, Example4)) {
		_a(t)
	}
	#[cfg(all(feature = "frame-feature-testing", not(feature = "frame-feature-testing-2")))]
	fn _b(t: (System, Example, Example2, Example3, Example4)) {
		_a(t)
	}
	#[cfg(all(not(feature = "frame-feature-testing"), feature = "frame-feature-testing-2"))]
	fn _b(t: (System, Example, Example2, Example4, Example5)) {
		_a(t)
	}
	#[cfg(all(feature = "frame-feature-testing", feature = "frame-feature-testing-2"))]
	fn _b(t: (System, Example, Example2, Example3, Example4, Example5)) {
		_a(t)
	}
}

#[test]
fn assert_type_all_pallets_without_system_is_correct() {
	// Just ensure the 2 types are same.
	fn _a(_t: AllPalletsWithoutSystem) {}
	#[cfg(all(not(feature = "frame-feature-testing"), not(feature = "frame-feature-testing-2")))]
	fn _b(t: (Example, Example2, Example4)) {
		_a(t)
	}
	#[cfg(all(feature = "frame-feature-testing", not(feature = "frame-feature-testing-2")))]
	fn _b(t: (Example, Example2, Example3, Example4)) {
		_a(t)
	}
	#[cfg(all(not(feature = "frame-feature-testing"), feature = "frame-feature-testing-2"))]
	fn _b(t: (Example, Example2, Example4, Example5)) {
		_a(t)
	}
	#[cfg(all(feature = "frame-feature-testing", feature = "frame-feature-testing-2"))]
	fn _b(t: (Example, Example2, Example3, Example4, Example5)) {
		_a(t)
	}
}

#[test]
fn test_storage_alias() {
	use frame_support::Twox64Concat;

	#[frame_support::storage_alias]
	type Value<T: pallet::Config>
	where
		<T as frame_system::Config>::AccountId: From<SomeType1> + SomeAssociation1,
	= StorageValue<pallet::Pallet<T>, u32, ValueQuery>;

	#[frame_support::storage_alias]
	type SomeCountedStorageMap<T: pallet2::Config>
	where
		<T as frame_system::Config>::AccountId: From<SomeType1> + SomeAssociation1,
	= CountedStorageMap<pallet2::Pallet<T>, Twox64Concat, u8, u32>;

	TestExternalities::default().execute_with(|| {
		pallet::Value::<Runtime>::put(10);
		assert_eq!(10, Value::<Runtime>::get());

		pallet2::SomeCountedStorageMap::<Runtime>::insert(10, 100);
		assert_eq!(Some(100), SomeCountedStorageMap::<Runtime>::get(10));
		assert_eq!(1, SomeCountedStorageMap::<Runtime>::count());
		assert_eq!(
			SomeCountedStorageMap::<Runtime>::storage_info(),
			pallet2::SomeCountedStorageMap::<Runtime>::storage_info()
		);
	})
}

#[test]
fn pallet_on_chain_storage_version_initializes_correctly() {
	type Executive = frame_executive::Executive<
		Runtime,
		Block,
		frame_system::ChainContext<Runtime>,
		Runtime,
		AllPalletsWithSystem,
	>;

	// Simple example of a pallet with in-code version 10 being added to the runtime for the first
	// time.
	TestExternalities::default().execute_with(|| {
		let in_code_version = Example::in_code_storage_version();

		// Check the pallet has no storage items set.
		let pallet_hashed_prefix = twox_128(Example::name().as_bytes());
		let exists = contains_prefixed_key(&pallet_hashed_prefix);
		assert_eq!(exists, false);

		// [`frame_support::traits::BeforeAllRuntimeMigrations`] hook should initialize the storage
		// version.
		Executive::execute_on_runtime_upgrade();

		// Check that the storage version was initialized to the in-code version
		let on_chain_version_after = StorageVersion::get::<Example>();
		assert_eq!(on_chain_version_after, in_code_version);
	});

	// Pallet with no in-code storage version should have the on-chain version initialized to 0.
	TestExternalities::default().execute_with(|| {
		// Example4 in_code_storage_version is NoStorageVersionSet.

		// Check the pallet has no storage items set.
		let pallet_hashed_prefix = twox_128(Example4::name().as_bytes());
		let exists = contains_prefixed_key(&pallet_hashed_prefix);
		assert_eq!(exists, false);

		// Confirm the storage version is implicitly 0.
		let on_chain_version_before = StorageVersion::get::<Example4>();
		assert_eq!(on_chain_version_before, StorageVersion::new(0));

		// [`frame_support::traits::BeforeAllRuntimeMigrations`] initializes the storage version.
		Executive::execute_on_runtime_upgrade();

		// Check that the storage version now exists and was initialized to 0.
		let on_chain_version_after = StorageVersion::get::<Example4>();
		assert_eq!(StorageVersion::exists::<Example4>(), true);
		assert_eq!(on_chain_version_after, StorageVersion::new(0));
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn post_runtime_upgrade_detects_storage_version_issues() {
	use frame_support::traits::UpgradeCheckSelect;

	struct CustomUpgrade;

	impl OnRuntimeUpgrade for CustomUpgrade {
		fn on_runtime_upgrade() -> Weight {
			Example2::in_code_storage_version().put::<Example2>();

			Default::default()
		}
	}

	struct CustomUpgradePallet4;

	impl OnRuntimeUpgrade for CustomUpgradePallet4 {
		fn on_runtime_upgrade() -> Weight {
			StorageVersion::new(100).put::<Example4>();

			Default::default()
		}
	}

	type Executive = frame_executive::Executive<
		Runtime,
		Block,
		frame_system::ChainContext<Runtime>,
		Runtime,
		AllPalletsWithSystem,
	>;

	/// TODO: The `OnRuntimeUpgrade` generic parameter in `Executive` is deprecated and will be
	/// removed in a future version. Once removed, this `#[allow(deprecated)]` attribute
	/// can be safely deleted.
	#[allow(deprecated)]
	type ExecutiveWithUpgrade = frame_executive::Executive<
		Runtime,
		Block,
		frame_system::ChainContext<Runtime>,
		Runtime,
		AllPalletsWithSystem,
		CustomUpgrade,
	>;

	/// TODO: The `OnRuntimeUpgrade` generic parameter in `Executive` is deprecated and will be
	/// removed in a future version. Once removed, this `#[allow(deprecated)]` attribute
	/// can be safely deleted.
	#[allow(deprecated)]
	type ExecutiveWithUpgradePallet4 = frame_executive::Executive<
		Runtime,
		Block,
		frame_system::ChainContext<Runtime>,
		Runtime,
		AllPalletsWithSystem,
		CustomUpgradePallet4,
	>;

	TestExternalities::default().execute_with(|| {
		// Set the on-chain version to one less than the in-code version for `Example`, simulating a
		// forgotten migration
		StorageVersion::new(9).put::<Example2>();

		// The version isn't changed, we should detect it.
		assert!(
			Executive::try_runtime_upgrade(UpgradeCheckSelect::PreAndPost).unwrap_err() ==
				"On chain and in-code storage version do not match. Missing runtime upgrade?"
					.into()
		);
	});

	TestExternalities::default().execute_with(|| {
		// Call `on_genesis` to put the storage version of `Example` into the storage.
		Example::on_genesis();
		// We set the new storage version in the pallet and that should be detected.
		UpdateStorageVersion::set(&true);
		Executive::try_runtime_upgrade(UpgradeCheckSelect::PreAndPost).unwrap();
	});

	TestExternalities::default().execute_with(|| {
		// Call `on_genesis` to put the storage version of `Example` into the storage.
		Example::on_genesis();
		// We set the new storage version in the custom upgrade and that should be detected.
		ExecutiveWithUpgrade::try_runtime_upgrade(UpgradeCheckSelect::PreAndPost).unwrap();
	});

	TestExternalities::default().execute_with(|| {
		// Call `on_genesis` to put the storage version of `Example` into the storage.
		Example::on_genesis();
		// We need to set the correct storage version for `Example2`
		UpdateStorageVersion::set(&true);

		// `CustomUpgradePallet4` will set a storage version for `Example4` while this doesn't has
		// any storage version "enabled".
		assert!(
			ExecutiveWithUpgradePallet4::try_runtime_upgrade(UpgradeCheckSelect::PreAndPost)
				.unwrap_err() ==
				"On chain storage version set, while the pallet \
				doesn't have the `#[pallet::storage_version(VERSION)]` attribute."
					.into()
		);
	});
}

#[test]
fn test_dispatch_context() {
	TestExternalities::default().execute_with(|| {
		// By default there is no context
		assert!(with_context::<(), _>(|_| ()).is_none());

		// When not using `dispatch`, there should be no dispatch context
		assert_eq!(
			DispatchError::Unavailable,
			Example::check_for_dispatch_context(RuntimeOrigin::root()).unwrap_err(),
		);

		// When using `dispatch`, there should be a dispatch context
		assert_ok!(RuntimeCall::from(pallet::Call::<Runtime>::check_for_dispatch_context {})
			.dispatch(RuntimeOrigin::root()));
	});
}

#[test]
fn test_call_feature_parsing() {
	let call = pallet::Call::<Runtime>::check_for_dispatch_context {};
	match call {
		pallet::Call::<Runtime>::check_for_dispatch_context {} |
		pallet::Call::<Runtime>::foo { .. } |
		pallet::Call::foo_storage_layer { .. } |
		pallet::Call::foo_index_out_of_order {} |
		pallet::Call::foo_no_post_info {} => (),
		#[cfg(feature = "frame-feature-testing")]
		pallet::Call::foo_feature_test {} => (),
		pallet::Call::__Ignore(_, _) => (),
	}
}

#[test]
#[allow(deprecated)]
fn test_error_feature_parsing() {
	let err = pallet::Error::<Runtime>::InsufficientProposersBalance;
	match err {
		pallet::Error::InsufficientProposersBalance |
		pallet::Error::NonExistentStorageValue |
		pallet::Error::Code(_) |
		pallet::Error::Skipped(_) |
		pallet::Error::CompactU8(_) => (),
		#[cfg(feature = "frame-feature-testing")]
		pallet::Error::FeatureTest => (),
		pallet::Error::__Ignore(_, _) => (),
	}
}

#[test]
fn pallet_metadata() {
	use sp_metadata_ir::{EnumDeprecationInfoIR, ItemDeprecationInfoIR, VariantDeprecationInfoIR};
	let pallets = Runtime::metadata_ir().pallets;
	let example = pallets[0].clone();
	let example2 = pallets[1].clone();
	{
		// Example pallet calls is fully and partially deprecated
		let meta = &example.calls.unwrap();
		assert_eq!(
			EnumDeprecationInfoIR(BTreeMap::from([(
				0,
				VariantDeprecationInfoIR::Deprecated { note: "test", since: None }
			)])),
			meta.deprecation_info
		)
	}
	{
		// Example pallet constant is deprecated
		let meta = &example.constants[0];
		assert_eq!(
			ItemDeprecationInfoIR::Deprecated { note: "test 2", since: None },
			meta.deprecation_info
		)
	}
	{
		// Example pallet errors are partially and fully deprecated
		let meta = &example.error.unwrap();
		assert_eq!(
			EnumDeprecationInfoIR(BTreeMap::from([(
				2,
				VariantDeprecationInfoIR::Deprecated { note: "test", since: None }
			)])),
			meta.deprecation_info
		)
	}
	{
		// Example pallet events are partially and fully deprecated
		let meta = example.event.unwrap();
		assert_eq!(
			EnumDeprecationInfoIR(BTreeMap::from([(
				1,
				VariantDeprecationInfoIR::Deprecated { note: "test", since: None }
			)])),
			meta.deprecation_info
		);
	}
	{
		// Example2 pallet events are not deprecated
		let meta = example2.event.unwrap();
		assert!(!meta.deprecation_info.has_deprecated_variants());
	}
}
