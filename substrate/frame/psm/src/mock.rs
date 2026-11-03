// This file is part of Substrate.

// Copyright (C) Amforc AG.
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

use frame_support::{
	derive_impl, parameter_types,
	traits::{
		fungible::HoldConsideration, AsEnsureOriginWithArg, Consideration, ConstU128, ConstU32,
		ConstU64, EitherOf, LinearStoragePrice,
	},
	weights::constants::RocksDbWeight,
	PalletId,
};
use frame_system::{
	mocking::MockBlock, EnsureRoot, EnsureRootWithSuccess, EnsureSigned, GenesisConfig,
};
use sp_io::TestExternalities as TestState;
use sp_runtime::{traits::IdentityLookup, AccountId32, BuildStorage, Permill};

pub type AccountId = AccountId32;

// Test accounts use the same 32-byte shape as production runtimes.
pub const ALICE: AccountId = AccountId::new([1; 32]);
pub const BOB: AccountId = AccountId::new([2; 32]);
pub const CHARLIE: AccountId = AccountId::new([3; 32]);
pub const INSURANCE_FUND: AccountId = AccountId::new([100; 32]);
/// Account whose signed origin acts as the emergency admin on the test PSM.
pub const EMERGENCY_ACCOUNT: AccountId = AccountId::new([99; 32]);

// Asset IDs
pub const INTERNAL_ASSET_ID: u32 = 1;
pub const USDC_ASSET_ID: u32 = 2;
pub const USDT_ASSET_ID: u32 = 3;
pub const USDX_ASSET_ID: u32 = 10;
pub const DAI_MOCK_ASSET_ID: u32 = 11;
pub const UNSUPPORTED_ASSET_ID: u32 = 99;

// internal unit (6 decimals)
pub const INTERNAL_UNIT: u128 = 1_000_000;
/// USDX has 2 decimals — fewer than internal.
pub const USDX_UNIT: u128 = 100;
/// DAI_MOCK has 18 decimals — more than internal.
pub const DAI_UNIT: u128 = 1_000_000_000_000_000_000;

// Initial balances for testing
pub const INITIAL_BALANCE: u128 = 1_000_000 * INTERNAL_UNIT; // 1M units
/// Default per-instance debt ceiling at genesis: 50% of the legacy 20M issuance cap.
pub const DEFAULT_MAX_DEBT: u128 = 10_000_000 * INTERNAL_UNIT;

#[frame_support::runtime]
mod test_runtime {
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
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;
	#[runtime::pallet_index(1)]
	pub type Balances = pallet_balances;
	#[runtime::pallet_index(2)]
	pub type Assets = pallet_assets;
	#[runtime::pallet_index(3)]
	pub type Psm = crate;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Nonce = u64;
	type Block = MockBlock<Test>;
	type BlockHashCount = ConstU64<250>;
	type DbWeight = RocksDbWeight;
	type AccountData = pallet_balances::AccountData<u128>;
	type AccountId = AccountId;
	type Lookup = IdentityLookup<Self::AccountId>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type ExistentialDeposit = ConstU128<1>;
	type AccountStore = System;
	type RuntimeHoldReason = RuntimeHoldReason;
}

#[derive_impl(pallet_assets::config_preludes::TestDefaultConfig)]
impl pallet_assets::Config for Test {
	type Balance = u128;
	type AssetId = u32;
	type AssetIdParameter = u32;
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
}

parameter_types! {
	pub const MinSwapAmount: u128 = 100 * INTERNAL_UNIT;
	pub const PsmPalletId: PalletId = PalletId(*b"py/psm!!");
	pub const PsmCreationDeposit: u128 = 1_000_000;
	// Non-zero so tests observe the footprint through the held amount; an under-reported
	// footprint in `create_psm` would go unnoticed with a slope of zero.
	pub const PsmDepositSlope: u128 = 100;
	pub PsmHoldReason: RuntimeHoldReason = RuntimeHoldReason::Psm(crate::HoldReason::CreationDeposit);
	pub const NoPsmDepositor: Option<AccountId> = None;
}

type PsmCreateOrigin =
	EitherOf<EnsureRootWithSuccess<AccountId, NoPsmDepositor>, crate::EnsureAssetOwner<Test>>;

#[cfg(feature = "runtime-benchmarks")]
pub struct PsmBenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl crate::BenchmarkHelper<u32, AccountId> for PsmBenchmarkHelper {
	fn get_asset_id(asset_index: u32) -> u32 {
		asset_index
	}
	fn create_asset(asset_id: u32, owner: &AccountId, decimals: u8) {
		use frame_support::traits::fungibles::{metadata::Mutate as MetadataMutate, Create};
		if !<Assets as frame_support::traits::fungibles::Inspect<AccountId>>::asset_exists(asset_id)
		{
			let _ = <Assets as Create<AccountId>>::create(asset_id, owner.clone(), true, 1);
		}
		// Fund the owner's native balance so they can pay the metadata deposit.
		let _ = Balances::force_set_balance(RuntimeOrigin::root(), owner.clone(), INITIAL_BALANCE);
		let _ = <Assets as MetadataMutate<AccountId>>::set(
			asset_id,
			owner,
			b"Benchmark".to_vec(),
			b"BNC".to_vec(),
			decimals,
		);
	}
}

impl crate::Config for Test {
	type Fungibles = Assets;
	type Consideration = HoldConsideration<
		AccountId,
		Balances,
		PsmHoldReason,
		LinearStoragePrice<PsmCreationDeposit, PsmDepositSlope, u128>,
	>;
	type CreateOrigin = PsmCreateOrigin;
	type RuntimeOrigin = RuntimeOrigin;
	type PalletsOrigin = OriginCaller;
	type AssetId = u32;
	type WeightInfo = ();
	type PalletId = PsmPalletId;
	type MaxExternals = ConstU32<10>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = PsmBenchmarkHelper;
}

pub fn new_test_ext() -> TestState {
	let mut storage = GenesisConfig::<Test>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<Test> {
		balances: vec![
			(ALICE, INITIAL_BALANCE),
			(BOB, INITIAL_BALANCE),
			(CHARLIE, INITIAL_BALANCE),
			(INSURANCE_FUND, 1),
		],
		..Default::default()
	}
	.assimilate_storage(&mut storage)
	.unwrap();

	pallet_assets::GenesisConfig::<Test> {
		assets: vec![
			(INTERNAL_ASSET_ID, ALICE, true, 1),
			(USDC_ASSET_ID, ALICE, true, 1),
			(USDT_ASSET_ID, ALICE, true, 1),
			(USDX_ASSET_ID, ALICE, true, 1),
			(DAI_MOCK_ASSET_ID, ALICE, true, 1),
		],
		metadata: vec![
			(INTERNAL_ASSET_ID, b"Internal Asset".to_vec(), b"INTERNAL".to_vec(), 6),
			(USDC_ASSET_ID, b"USD Coin".to_vec(), b"USDC".to_vec(), 6),
			(USDT_ASSET_ID, b"Tether USD".to_vec(), b"USDT".to_vec(), 6),
			(USDX_ASSET_ID, b"Low-Decimal Coin".to_vec(), b"USDX".to_vec(), 2),
			(DAI_MOCK_ASSET_ID, b"Dai Stablecoin".to_vec(), b"DAI".to_vec(), 18),
		],
		accounts: vec![
			(USDC_ASSET_ID, ALICE, 10_000 * INTERNAL_UNIT),
			(USDC_ASSET_ID, BOB, 10_000 * INTERNAL_UNIT),
			(USDT_ASSET_ID, ALICE, 10_000 * INTERNAL_UNIT),
			(USDT_ASSET_ID, BOB, 10_000 * INTERNAL_UNIT),
			(USDX_ASSET_ID, ALICE, 10_000 * USDX_UNIT),
			(USDX_ASSET_ID, BOB, 10_000 * USDX_UNIT),
			(DAI_MOCK_ASSET_ID, ALICE, 10_000 * DAI_UNIT),
			(DAI_MOCK_ASSET_ID, BOB, 10_000 * DAI_UNIT),
		],
		..Default::default()
	}
	.assimilate_storage(&mut storage)
	.unwrap();

	let mut ext: TestState = storage.into();

	ext.execute_with(|| {
		System::set_block_number(1);
		install_test_psm();
	});

	ext
}

/// Direct storage install of the test PSM with `Root` as full_admin and
/// `Signed(EMERGENCY_ACCOUNT)` as emergency_admin. We bypass `create_psm` here so tests don't
/// depend on balance funding plumbing.
fn install_test_psm() {
	let internal_decimals = <Assets as frame_support::traits::fungibles::metadata::Inspect<
		AccountId,
	>>::decimals(INTERNAL_ASSET_ID);
	let full_admin: OriginCaller = frame_system::RawOrigin::<AccountId>::Root.into();
	let emergency_admin: OriginCaller =
		frame_system::RawOrigin::<AccountId>::Signed(EMERGENCY_ACCOUNT).into();
	crate::Psm::<Test>::insert(
		INTERNAL_ASSET_ID,
		crate::PsmInfo::<Test> {
			fee_destination: INSURANCE_FUND,
			max_debt: DEFAULT_MAX_DEBT,
			min_swap_amount: 100 * INTERNAL_UNIT,
			internal_decimals,
			external_count: 2,
		},
	);
	let ticket = <Test as crate::Config>::Consideration::new(
		&ALICE,
		crate::Pallet::<Test>::psm_creation_footprint(),
	)
	.expect("ALICE is funded; consideration succeeds");
	crate::PsmAdmin::<Test>::insert(
		INTERNAL_ASSET_ID,
		crate::PsmAdminInfo::<Test> { full_admin, emergency_admin, deposit: Some((ALICE, ticket)) },
	);
	// Acquire provider refs like `create_psm` does, so the test PSM mirrors a real one.
	frame_system::Pallet::<Test>::inc_providers(&crate::Pallet::<Test>::psm_account(
		&INTERNAL_ASSET_ID,
	));
	frame_system::Pallet::<Test>::inc_providers(&INSURANCE_FUND);

	for (asset, weight, decimals) in [
		(USDC_ASSET_ID, Permill::from_percent(60), 6u8),
		(USDT_ASSET_ID, Permill::from_percent(40), 6u8),
	] {
		crate::ExternalAssets::<Test>::insert(
			INTERNAL_ASSET_ID,
			asset,
			crate::ExternalAssetInfo { status: crate::CircuitBreakerLevel::AllEnabled, decimals },
		);
		crate::MintingFee::<Test>::insert(INTERNAL_ASSET_ID, asset, Permill::from_percent(1));
		crate::RedemptionFee::<Test>::insert(INTERNAL_ASSET_ID, asset, Permill::from_percent(1));
		crate::AssetCeilingWeight::<Test>::insert(INTERNAL_ASSET_ID, asset, weight);
	}
}

pub struct ExtBuilder {
	mint_ops: Vec<(AccountId, u32, u128)>,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self { mint_ops: vec![] }
	}
}

impl ExtBuilder {
	/// Queue a PSM mint: `who` mints `amount` of USDC.
	pub fn mints(self, who: AccountId, amount: u128) -> Self {
		self.mints_asset(who, USDC_ASSET_ID, amount)
	}

	/// Queue a PSM mint of a specific asset.
	pub fn mints_asset(mut self, who: AccountId, asset_id: u32, amount: u128) -> Self {
		self.mint_ops.push((who, asset_id, amount));
		self
	}

	pub fn build_and_execute(self, test: impl FnOnce()) {
		new_test_ext().execute_with(|| {
			for (who, asset_id, amount) in self.mint_ops {
				frame_support::assert_ok!(crate::Pallet::<Test>::mint(
					RuntimeOrigin::signed(who),
					INTERNAL_ASSET_ID,
					asset_id,
					amount,
					crate::MintingFee::<Test>::get(INTERNAL_ASSET_ID, asset_id),
				));
			}
			test();
			crate::Pallet::<Test>::do_try_state().expect("try_state post-condition failed");
		});
	}
}

pub fn set_minting_fee(asset_id: u32, fee: Permill) {
	crate::MintingFee::<Test>::insert(INTERNAL_ASSET_ID, asset_id, fee);
}

pub fn set_redemption_fee(asset_id: u32, fee: Permill) {
	crate::RedemptionFee::<Test>::insert(INTERNAL_ASSET_ID, asset_id, fee);
}

pub fn set_max_debt(value: u128) {
	crate::Psm::<Test>::mutate(INTERNAL_ASSET_ID, |maybe| {
		if let Some(info) = maybe.as_mut() {
			info.max_debt = value;
		}
	});
}

pub fn set_asset_ceiling_weight(asset_id: u32, weight: Permill) {
	crate::AssetCeilingWeight::<Test>::insert(INTERNAL_ASSET_ID, asset_id, weight);
}

pub fn set_asset_status(asset_id: u32, status: crate::CircuitBreakerLevel) {
	crate::ExternalAssets::<Test>::mutate(INTERNAL_ASSET_ID, asset_id, |maybe| {
		if let Some(info) = maybe.as_mut() {
			info.status = status;
		}
	});
}

/// Register an external asset via the extrinsic (records snapshot decimals) and
/// assign it a per-asset ceiling weight.
pub fn register_external_asset_with_weight(asset_id: u32, weight: Permill) {
	use frame_support::assert_ok;
	assert_ok!(crate::Pallet::<Test>::add_external_asset(
		RuntimeOrigin::root(),
		INTERNAL_ASSET_ID,
		asset_id,
	));
	assert_ok!(crate::Pallet::<Test>::set_asset_ceiling_weight(
		RuntimeOrigin::root(),
		INTERNAL_ASSET_ID,
		asset_id,
		weight,
	));
}

pub fn fund_external_asset(asset_id: u32, account: AccountId, amount: u128) {
	use frame_support::traits::fungibles::Mutate;
	let _ = Assets::mint_into(asset_id, &account, amount);
}

pub fn fund_internal(account: AccountId, amount: u128) {
	use frame_support::traits::fungibles::Mutate;
	let _ = Assets::mint_into(INTERNAL_ASSET_ID, &account, amount);
}

pub fn create_asset_with_metadata(asset_id: u32) {
	use frame_support::assert_ok;
	assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), asset_id, ALICE, 1));
	assert_ok!(Assets::set_metadata(
		RuntimeOrigin::signed(ALICE),
		asset_id,
		b"Test Asset".to_vec(),
		b"TST".to_vec(),
		6
	));
}

pub fn get_asset_balance(asset_id: u32, account: AccountId) -> u128 {
	Assets::balance(asset_id, account)
}

pub fn psm_account() -> AccountId {
	crate::Pallet::<Test>::psm_account(&INTERNAL_ASSET_ID)
}
