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

mod block_hash;
mod deposit_payment;
mod eth_estimate_gas;
mod pallet_dummy;
mod precompiles;
mod pvm;
mod sol;
mod stipends;

use std::collections::HashMap;

use crate::{
	self as pallet_revive, AccountId32Mapper, AddressMapper, BalanceOf, BalanceWithDust, Call,
	CodeInfoOf, Config, DelegateInfo, ExecOrigin as Origin, ExecReturnValue, GenesisConfig,
	OriginFor, Pallet, PristineCode,
	deposit_payment::PGasDeposit,
	evm::{
		fees::{BlockRatioFee, Info as FeeInfo},
		runtime::{EthExtra, SetWeightLimit},
	},
	genesis::{Account, ContractData},
	mock::MockHandler,
	test_utils::*,
};
use frame_support::{
	DefaultNoBound, assert_ok, derive_impl,
	pallet_prelude::EnsureOrigin,
	parameter_types,
	traits::{
		AsEnsureOriginWithArg, ConstU32, ConstU128, FindAuthor, OriginTrait, StorageVersion,
		tokens::imbalance::ResolveTo,
	},
	weights::{FixedFee, Weight, constants::WEIGHT_REF_TIME_PER_SECOND},
};
use pallet_revive_fixtures::compile_module;
use pallet_transaction_payment::{ChargeTransactionPayment, ConstFeeMultiplier, Multiplier};
use sp_core::{H160, U256};
use sp_keystore::{KeystoreExt, testing::MemoryKeystore};
use sp_runtime::{
	AccountId32, BuildStorage, FixedU128, MultiAddress, MultiSignature, Perbill, Storage,
	generic::Header,
	traits::{BlakeTwo256, Convert, IdentityLookup, One},
};

pub type Address = MultiAddress<AccountId32, u32>;
pub type Block = sp_runtime::generic::Block<Header<u64, BlakeTwo256>, UncheckedExtrinsic>;
pub type Signature = MultiSignature;
pub type SignedExtra = (
	frame_system::CheckNonce<Test>,
	ChargeTransactionPayment<Test>,
	crate::evm::tx_extension::SetOrigin<Test>,
);
pub type UncheckedExtrinsic =
	crate::evm::runtime::UncheckedExtrinsic<Address, Signature, EthExtraImpl>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EthExtraImpl;

impl EthExtra for EthExtraImpl {
	type Config = Test;
	type ExtensionV0 = SignedExtra;
	type ExtensionOtherVersions = sp_runtime::traits::InvalidVersion;

	fn get_eth_extension(nonce: u32, tip: BalanceOf<Test>) -> Self::ExtensionV0 {
		(
			frame_system::CheckNonce::from(nonce),
			ChargeTransactionPayment::from(tip),
			crate::evm::tx_extension::SetOrigin::<Test>::new_from_eth_transaction(),
		)
	}
}

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		Balances: pallet_balances,
		Timestamp: pallet_timestamp,
		Utility: pallet_utility,
		Contracts: pallet_revive,
		Proxy: pallet_proxy,
		TransactionPayment: pallet_transaction_payment,
		Assets: pallet_assets,
		AssetsHolder: pallet_assets_holder,
		AssetsFreezer: pallet_assets_freezer,
		Dummy: pallet_dummy
	}
);

#[macro_export]
macro_rules! assert_return_code {
	( $x:expr , $y:expr $(,)? ) => {{
		assert_eq!(u32::from_le_bytes($x.data[..].try_into().unwrap()), $y as u32);
	}};
}

#[macro_export]
macro_rules! assert_refcount {
	( $code_hash:expr , $should:expr $(,)? ) => {{
		let is = crate::CodeInfoOf::<Test>::get($code_hash).map(|m| m.refcount()).unwrap();
		assert_eq!(is, $should);
	}};
}

pub mod test_utils {
	use super::{
		BalanceWithDust, CodeHashLockupDepositPercent, Contracts, DepositPerByte, DepositPerItem,
		Test,
	};
	use crate::{
		AccountInfo, AccountInfoOf, BalanceOf, CodeInfo, CodeInfoOf, Config, ContractInfo,
		PristineCode, address::AddressMapper, exec::AccountIdOf,
	};
	use codec::{Encode, MaxEncodedLen};
	use frame_support::traits::fungible::{InspectHold, Mutate};
	use sp_core::H160;

	pub fn place_contract(address: &AccountIdOf<Test>, code_hash: sp_core::H256) {
		set_balance(address, Contracts::min_balance() * 10);
		<CodeInfoOf<Test>>::insert(code_hash, CodeInfo::new(address.clone()));
		let address =
			<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_address(&address);
		let contract = <ContractInfo<Test>>::new(&address, 0, code_hash).unwrap();
		AccountInfo::<Test>::insert_contract(&address, contract);
	}
	pub fn set_balance(who: &AccountIdOf<Test>, amount: u128) {
		let _ = <Test as Config>::Currency::set_balance(who, amount);
	}
	pub fn get_balance(who: &AccountIdOf<Test>) -> u128 {
		<Test as Config>::Currency::free_balance(who)
	}
	pub fn get_balance_on_hold(
		reason: &<Test as Config>::RuntimeHoldReason,
		who: &AccountIdOf<Test>,
	) -> u128 {
		<Test as Config>::Currency::balance_on_hold(reason.into(), who)
	}
	pub fn get_contract(addr: &H160) -> ContractInfo<Test> {
		get_contract_checked(addr).unwrap()
	}
	pub fn get_contract_checked(addr: &H160) -> Option<ContractInfo<Test>> {
		AccountInfo::<Test>::load_contract(addr)
	}
	pub fn get_code_deposit(code_hash: &sp_core::H256) -> BalanceOf<Test> {
		crate::CodeInfoOf::<Test>::get(code_hash).unwrap().deposit()
	}
	pub fn lockup_deposit(code_hash: &sp_core::H256) -> BalanceOf<Test> {
		CodeHashLockupDepositPercent::get().mul_ceil(get_code_deposit(code_hash)).into()
	}
	pub fn contract_base_deposit(addr: &H160) -> BalanceOf<Test> {
		let contract_info = self::get_contract(&addr);
		let info_size = contract_info.encoded_size() as u128;
		let code_deposit = CodeHashLockupDepositPercent::get()
			.mul_ceil(get_code_deposit(&contract_info.code_hash));
		let deposit = DepositPerByte::get()
			.saturating_mul(info_size)
			.saturating_add(DepositPerItem::get())
			.saturating_add(code_deposit);
		let immutable_size = contract_info.immutable_data_len() as u128;
		if immutable_size > 0 {
			let immutable_deposit = DepositPerByte::get()
				.saturating_mul(immutable_size)
				.saturating_add(DepositPerItem::get());
			deposit.saturating_add(immutable_deposit)
		} else {
			deposit
		}
	}
	pub fn expected_deposit(code_len: usize) -> u128 {
		// For code_info, the deposit for max_encoded_len is taken.
		let code_info_len = CodeInfo::<Test>::max_encoded_len() as u128;
		// Calculate deposit to be reserved.
		// We add 2 storage items: one for code, other for code_info
		DepositPerByte::get().saturating_mul(code_len as u128 + code_info_len) +
			DepositPerItem::get().saturating_mul(2)
	}
	pub fn ensure_stored(code_hash: sp_core::H256) -> usize {
		// Assert that code_info is stored
		assert!(CodeInfoOf::<Test>::contains_key(&code_hash));
		// Assert that contract code is stored, and get its size.
		PristineCode::<Test>::try_get(&code_hash).unwrap().len()
	}
	pub fn u256_bytes(u: u64) -> [u8; 32] {
		let mut buffer = [0u8; 32];
		let bytes = u.to_le_bytes();
		buffer[..8].copy_from_slice(&bytes);
		buffer
	}

	pub fn set_balance_with_dust(address: &H160, value: BalanceWithDust<BalanceOf<Test>>) {
		use frame_support::traits::Currency;
		let ed = <Test as Config>::Currency::minimum_balance();
		let (value, dust) = value.deconstruct();
		let account_id = <Test as Config>::AddressMapper::to_account_id(&address);
		<Test as Config>::Currency::set_balance(&account_id, ed + value);
		if dust > 0 {
			AccountInfoOf::<Test>::mutate(&address, |account| {
				if let Some(account) = account {
					account.dust = dust;
				} else {
					*account = Some(AccountInfo { dust, ..Default::default() });
				}
			});
		}
	}
}

pub(crate) mod builder {
	use super::Test;
	use crate::{
		Code,
		test_utils::{ALICE, builder::*},
		tests::RuntimeOrigin,
	};
	use sp_core::{H160, H256};

	pub fn bare_instantiate(code: Code) -> BareInstantiateBuilder<Test> {
		BareInstantiateBuilder::<Test>::bare_instantiate(RuntimeOrigin::signed(ALICE), code)
	}

	pub fn bare_call(dest: H160) -> BareCallBuilder<Test> {
		BareCallBuilder::<Test>::bare_call(RuntimeOrigin::signed(ALICE), dest)
	}

	pub fn instantiate_with_code(code: Vec<u8>) -> InstantiateWithCodeBuilder<Test> {
		InstantiateWithCodeBuilder::<Test>::instantiate_with_code(
			RuntimeOrigin::signed(ALICE),
			code,
		)
	}

	pub fn instantiate(code_hash: H256) -> InstantiateBuilder<Test> {
		InstantiateBuilder::<Test>::instantiate(RuntimeOrigin::signed(ALICE), code_hash)
	}

	pub fn call(dest: H160) -> CallBuilder<Test> {
		CallBuilder::<Test>::call(RuntimeOrigin::signed(ALICE), dest)
	}

	pub fn eth_call(dest: H160) -> EthCallBuilder<Test> {
		EthCallBuilder::<Test>::eth_call(crate::Origin::<Test>::EthTransaction(ALICE).into(), dest)
	}

	pub fn eth_instantiate_with_code(code: Vec<u8>) -> EthInstantiateWithCodeBuilder<Test> {
		EthInstantiateWithCodeBuilder::<Test>::eth_instantiate_with_code(
			crate::Origin::<Test>::EthTransaction(ALICE).into(),
			code,
		)
	}
}

impl Test {
	pub fn set_allow_evm_bytecode(allow_evm_bytecode: bool) {
		ALLOW_EVM_BYTECODE.with(|v| *v.borrow_mut() = allow_evm_bytecode);
	}
}

parameter_types! {
	pub BlockWeights: frame_system::limits::BlockWeights =
		frame_system::limits::BlockWeights::simple_max(
			Weight::from_parts(2 * WEIGHT_REF_TIME_PER_SECOND, 10 * 1024 * 1024),
		);
	pub static ExistentialDeposit: u128 = 1;
	// Non-zero, so tests fail when a DB access is not charged.
	pub const TestDbWeight: frame_support::weights::RuntimeDbWeight =
		frame_support::weights::RuntimeDbWeight { read: 100, write: 300 };
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type BlockWeights = BlockWeights;
	type DbWeight = TestDbWeight;
	type AccountId = AccountId32;
	type Lookup = IdentityLookup<Self::AccountId>;
	type AccountData = pallet_balances::AccountData<u128>;
	type OnNewAccount = crate::AutoMapper<Test>;
	type OnKilledAccount = crate::AutoMapper<Test>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type ExistentialDeposit = ExistentialDeposit;
	type ReserveIdentifier = [u8; 8];
	type AccountStore = System;
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type FreezeIdentifier = RuntimeFreezeReason;
	type MaxFreezes = frame_support::traits::VariantCountOf<RuntimeFreezeReason>;
}

#[derive_impl(pallet_timestamp::config_preludes::TestDefaultConfig)]
impl pallet_timestamp::Config for Test {}

impl pallet_utility::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type PalletsOrigin = OriginCaller;
	type WeightInfo = ();
}

impl pallet_proxy::Config for Test {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeCall = RuntimeCall;
	type Currency = Balances;
	type ProxyType = ();
	type ProxyDepositBase = ConstU128<1>;
	type ProxyDepositFactor = ConstU128<1>;
	type MaxProxies = ConstU32<32>;
	type WeightInfo = ();
	type MaxPending = ConstU32<32>;
	type CallHasher = BlakeTwo256;
	type AnnouncementDepositBase = ConstU128<1>;
	type AnnouncementDepositFactor = ConstU128<1>;
	type BlockNumberProvider = frame_system::Pallet<Test>;
}

parameter_types! {
	pub FeeMultiplier: Multiplier = Multiplier::one();
}

#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for Test {
	type OnChargeTransaction = pallet_transaction_payment::FungibleAdapter<Balances, ()>;
	type WeightToFee = BlockRatioFee<2, 1, Self, u128>;
	type LengthToFee = FixedFee<100, <Self as pallet_balances::Config>::Balance>;
	type FeeMultiplierUpdate = ConstFeeMultiplier<FeeMultiplier>;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config for Test {
	type Balance = u128;
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId32>>;
	type ForceOrigin = frame_system::EnsureRoot<AccountId32>;
	type Holder = AssetsHolder;
	type Freezer = AssetsFreezer;
}

impl pallet_assets_holder::Config for Test {
	type RuntimeHoldReason = RuntimeHoldReason;
	type RuntimeEvent = RuntimeEvent;
}

impl pallet_assets_freezer::Config for Test {
	type RuntimeFreezeReason = RuntimeFreezeReason;
	type RuntimeEvent = RuntimeEvent;
}

/// The PGAS asset id used by the test runtime.
pub const PGAS_ASSET_ID: u32 = 42;

parameter_types! {
	pub const PGasAssetId: u32 = PGAS_ASSET_ID;
	/// 10% of PGAS storage deposits are refunded, the rest is burned so users can't harvest
	/// free PGAS allowance from storage churn.
	pub const PGasRefundPercent: Perbill = Perbill::from_percent(10);
}

impl pallet_dummy::Config for Test {}

parameter_types! {
	pub static DepositPerByte: BalanceOf<Test> = 1;
	pub const DepositPerItem: BalanceOf<Test> = 2;
	pub const CodeHashLockupDepositPercent: Perbill = Perbill::from_percent(30);
	pub static ChainId: u64 = 448;
}

impl Convert<Weight, BalanceOf<Self>> for Test {
	fn convert(w: Weight) -> BalanceOf<Self> {
		w.ref_time().into()
	}
}

parameter_types! {
	pub static UploadAccount: Option<<Test as frame_system::Config>::AccountId> = None;
	pub static InstantiateAccount: Option<<Test as frame_system::Config>::AccountId> = None;
}

pub struct EnsureAccount<T, A>(core::marker::PhantomData<(T, A)>);
impl<T: Config, A: sp_core::Get<Option<crate::AccountIdOf<T>>>>
	EnsureOrigin<<T as frame_system::Config>::RuntimeOrigin> for EnsureAccount<T, A>
where
	<T as frame_system::Config>::AccountId: From<AccountId32>,
{
	type Success = T::AccountId;

	fn try_origin(o: OriginFor<T>) -> Result<Self::Success, OriginFor<T>> {
		let who = <frame_system::EnsureSigned<_> as EnsureOrigin<_>>::try_origin(o.clone())?;
		if matches!(A::get(), Some(a) if who != a) {
			return Err(o);
		}

		Ok(who)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin() -> Result<OriginFor<T>, ()> {
		Err(())
	}
}
parameter_types! {
	pub static AllowEvmBytecode: bool = true;
	pub CheckingAccount: AccountId32 = BOB.clone();
	pub BurnDestination: AccountId32 = AccountId32::new([42u8; 32]);
	pub static DebugFlag: bool = false;
	pub static AutoMapFlag: bool = false;
}

impl FindAuthor<<Test as frame_system::Config>::AccountId> for Test {
	fn find_author<'a, I>(_digests: I) -> Option<<Test as frame_system::Config>::AccountId>
	where
		I: 'a + IntoIterator<Item = (frame_support::ConsensusEngineId, &'a [u8])>,
	{
		Some(EVE)
	}
}

#[derive_impl(crate::config_preludes::TestDefaultConfig)]
impl Config for Test {
	type Time = Timestamp;
	type AddressMapper = AccountId32Mapper<Self>;
	type Balance = u128;
	type Currency = Balances;
	type DepositPerByte = DepositPerByte;
	type DepositPerItem = DepositPerItem;
	type DepositPerChildTrieItem = DepositPerItem;
	type AllowEVMBytecode = AllowEvmBytecode;
	type UploadOrigin = EnsureAccount<Self, UploadAccount>;
	type InstantiateOrigin = EnsureAccount<Self, InstantiateAccount>;
	type CodeHashLockupDepositPercent = CodeHashLockupDepositPercent;
	type ChainId = ChainId;
	type FindAuthor = Test;
	type Precompiles = (precompiles::WithInfo<Self>, precompiles::NoInfo<Self>);
	type FeeInfo = FeeInfo<Address, Signature, EthExtraImpl>;
	type Deposit =
		PGasDeposit<Test, Assets, AssetsHolder, AssetsFreezer, PGasAssetId, PGasRefundPercent>;
	type DebugEnabled = DebugFlag;
	type AutoMap = AutoMapFlag;
	type OnBurn = ResolveTo<BurnDestination, Balances>;
}

impl TryFrom<RuntimeCall> for Call<Test> {
	type Error = ();

	fn try_from(value: RuntimeCall) -> Result<Self, Self::Error> {
		match value {
			RuntimeCall::Contracts(call) => Ok(call),
			_ => Err(()),
		}
	}
}

impl SetWeightLimit for RuntimeCall {
	fn set_weight_limit(&mut self, new_weight_limit: Weight) -> Weight {
		match self {
			Self::Contracts(
				Call::eth_call { weight_limit, .. } |
				Call::eth_instantiate_with_code { weight_limit, .. },
			) => {
				let old = *weight_limit;
				*weight_limit = new_weight_limit;
				old
			},
			_ => Default::default(),
		}
	}
}

pub struct ExtBuilder {
	existential_deposit: u128,
	storage_version: Option<StorageVersion>,
	code_hashes: Vec<sp_core::H256>,
	genesis_config: Option<crate::GenesisConfig<Test>>,
	genesis_state_overrides: Option<Storage>,
	next_fee_multiplier: Option<FixedU128>,
	pgas_balances: Vec<(AccountId32, u128)>,
	pgas_min_balance: u128,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self {
			existential_deposit: ExistentialDeposit::get(),
			storage_version: None,
			code_hashes: vec![],
			genesis_config: Some(crate::GenesisConfig::<Test>::default()),
			genesis_state_overrides: None,
			next_fee_multiplier: None,
			pgas_balances: vec![],
			pgas_min_balance: 1,
		}
	}
}

impl ExtBuilder {
	/// The pallet genesis config to use, or None if you don't want to include it.
	pub fn genesis_config(mut self, config: Option<crate::GenesisConfig<Test>>) -> Self {
		self.genesis_config = config;
		self
	}
	pub fn existential_deposit(mut self, existential_deposit: u128) -> Self {
		self.existential_deposit = existential_deposit;
		self
	}
	pub fn with_code_hashes(mut self, code_hashes: Vec<sp_core::H256>) -> Self {
		self.code_hashes = code_hashes;
		self
	}
	pub fn with_next_fee_multiplier(mut self, next_fee_multiplier: FixedU128) -> Self {
		self.next_fee_multiplier = Some(next_fee_multiplier);
		self
	}
	/// Endow the given accounts with PGAS at genesis. The PGAS asset is always
	/// created; this just seeds initial balances.
	pub fn with_pgas_balances(mut self, balances: Vec<(AccountId32, u128)>) -> Self {
		self.pgas_balances = balances;
		self
	}
	/// Override the PGAS asset's `min_balance` (existential deposit).
	pub fn with_pgas_min_balance(mut self, min_balance: u128) -> Self {
		self.pgas_min_balance = min_balance;
		self
	}
	pub fn set_associated_consts(&self) {
		EXISTENTIAL_DEPOSIT.with(|v| *v.borrow_mut() = self.existential_deposit);
	}
	pub fn with_genesis_state_overrides(mut self, storage: Storage) -> Self {
		self.genesis_state_overrides = Some(storage);
		self
	}
	pub fn build(self) -> sp_io::TestExternalities {
		sp_tracing::try_init_simple();
		self.set_associated_consts();
		let mut t = self.genesis_state_overrides.unwrap_or_default();

		frame_system::GenesisConfig::<Test>::default()
			.assimilate_storage(&mut t)
			.unwrap();

		let checking_account = Pallet::<Test>::checking_account();

		pallet_balances::GenesisConfig::<Test> {
			balances: vec![(checking_account.clone(), 1_000_000_000_000)],
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		pallet_assets::GenesisConfig::<Test> {
			assets: vec![(PGAS_ASSET_ID, ALICE, true, self.pgas_min_balance)],
			accounts: self
				.pgas_balances
				.iter()
				.map(|(who, bal)| (PGAS_ASSET_ID, who.clone(), *bal))
				.collect(),
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		if let Some(multiplier) = self.next_fee_multiplier {
			pallet_transaction_payment::GenesisConfig::<Test> { multiplier, ..Default::default() }
				.assimilate_storage(&mut t)
				.unwrap();
		}

		if let Some(genesis_config) = self.genesis_config {
			genesis_config.assimilate_storage(&mut t).unwrap();
		}
		let mut ext = sp_io::TestExternalities::new(t);
		ext.register_extension(KeystoreExt::new(MemoryKeystore::new()));
		ext.execute_with(|| {
			use frame_support::traits::OnGenesis;

			Pallet::<Test>::on_genesis();
			if let Some(storage_version) = self.storage_version {
				storage_version.put::<Pallet<Test>>();
			}
			System::set_block_number(1)
		});
		ext.execute_with(|| {
			for code_hash in self.code_hashes {
				CodeInfoOf::<Test>::insert(code_hash, crate::CodeInfo::new(ALICE));
			}
		});
		ext.execute_with(|| {
			assert_ok!(Pallet::<Test>::map_account(RuntimeOrigin::signed(checking_account)));
		});
		ext
	}
}

fn initialize_block(number: u64) {
	System::reset_events();
	System::initialize(&number, &[0u8; 32].into(), &Default::default());
}

impl Default for Origin<Test> {
	fn default() -> Self {
		Self::Signed(ALICE)
	}
}

/// Dummy EVM bytecode for mocked addresses.
/// This is minimal EVM bytecode (STOP) that terminates successfully.
pub const MOCK_CODE: [u8; 1] = [0x00];

/// A mock handler implementation for testing purposes.
#[derive(DefaultNoBound)]
pub struct MockHandlerImpl<T: crate::pallet::Config> {
	// Always return this caller if set.
	mock_caller: Option<H160>,
	// Map of callee address to mocked call return value.
	mock_call: HashMap<H160, ExecReturnValue>,
	// Map of input data to mocked delegated caller info.
	mock_delegate_caller: HashMap<Vec<u8>, DelegateInfo<T>>,
}

impl<T: crate::pallet::Config> MockHandler<T> for MockHandlerImpl<T> {
	fn mock_caller(&self, _frames_len: usize) -> Option<OriginFor<T>> {
		self.mock_caller.as_ref().map(|mock_caller| {
			OriginFor::<T>::signed(T::AddressMapper::to_fallback_account_id(mock_caller))
		})
	}

	fn mock_call(
		&self,
		_callee: H160,
		_call_data: &[u8],
		_value_transferred: U256,
	) -> Option<ExecReturnValue> {
		self.mock_call.get(&_callee).cloned()
	}

	fn mock_delegated_caller(&self, _dest: H160, input_data: &[u8]) -> Option<DelegateInfo<T>> {
		self.mock_delegate_caller.get(&input_data.to_vec()).cloned()
	}

	fn mocked_code(&self, address: H160) -> Option<&[u8]> {
		if self.mock_call.contains_key(&address) { Some(&MOCK_CODE) } else { None }
	}
}

#[test]
fn ext_builder_with_genesis_config_works() {
	let pvm_contract = Account {
		address: crate::H160::repeat_byte(0x42),
		balance: U256::from(100_000_100),
		nonce: 42,
		contract_data: Some(ContractData {
			code: compile_module("dummy").unwrap().0.into(),
			storage: [([1u8; 32].into(), [2u8; 32].into())].into_iter().collect(),
		}),
	};

	let evm_contract = Account {
		address: crate::H160::repeat_byte(0x43),
		balance: U256::from(1_000_00_100),
		nonce: 43,
		contract_data: Some(ContractData {
			code: vec![
				revm::bytecode::opcode::PUSH1,
				0x00,
				revm::bytecode::opcode::PUSH1,
				0x00,
				revm::bytecode::opcode::RETURN,
			]
			.into(),
			storage: [([3u8; 32].into(), [4u8; 32].into())].into_iter().collect(),
		}),
	};

	let eoa =
		Account { address: ALICE_ADDR, balance: U256::from(100), nonce: 44, contract_data: None };

	let config = GenesisConfig::<Test> {
		mapped_accounts: vec![EVE],
		accounts: vec![eoa.clone(), pvm_contract.clone(), evm_contract.clone()],
		..Default::default()
	};

	// Genesis serialization works
	let json = serde_json::to_string(&config).unwrap();
	assert_eq!(config, serde_json::from_str::<GenesisConfig<Test>>(&json).unwrap());

	ExtBuilder::default().genesis_config(Some(config)).build().execute_with(|| {
		// account is mapped
		assert!(<Test as Config>::AddressMapper::is_mapped(&EVE));

		// EOA is created
		assert_eq!(Pallet::<Test>::evm_balance(&eoa.address), eoa.balance);

		// Contract is created
		for contract in [pvm_contract, evm_contract] {
			let contract_data = contract.contract_data.unwrap();
			let contract_info = test_utils::get_contract(&contract.address);

			assert!(System::account_exists(&<Test as Config>::AddressMapper::to_account_id(
				&contract.address
			)));

			assert_eq!(
				PristineCode::<Test>::get(&contract_info.code_hash).unwrap(),
				contract_data.code.0
			);
			assert_eq!(Pallet::<Test>::evm_nonce(&contract.address), contract.nonce);
			assert_eq!(Pallet::<Test>::evm_balance(&contract.address), contract.balance);

			for (key, value) in contract_data.storage.iter() {
				assert_eq!(
					Pallet::<Test>::get_storage(contract.address, key.0),
					Ok(Some(value.0.to_vec()))
				);
			}

			// Check that we can call contract created at genesis
			let result = builder::bare_call(contract.address).build_and_unwrap_result();
			assert!(!result.did_revert());
		}
	});
}
