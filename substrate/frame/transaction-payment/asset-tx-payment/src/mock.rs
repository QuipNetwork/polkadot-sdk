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

use super::*;
use crate as pallet_asset_tx_payment;

use codec;
use frame_support::{
	derive_impl,
	dispatch::DispatchClass,
	pallet_prelude::*,
	parameter_types,
	traits::{AsEnsureOriginWithArg, ConstU32, ConstU64, ConstU8, FindAuthor},
	weights::{Weight, WeightToFee as WeightToFeeT},
	ConsensusEngineId,
};
use frame_system as system;
use frame_system::EnsureRoot;
use pallet_transaction_payment::FungibleAdapter;
use sp_runtime::traits::{ConvertInto, SaturatedConversion};

type Block = frame_system::mocking::MockBlock<Runtime>;
type Balance = u64;
type AccountId = u64;

frame_support::construct_runtime!(
	pub enum Runtime {
		System: system,
		Balances: pallet_balances,
		TransactionPayment: pallet_transaction_payment,
		Assets: pallet_assets,
		Authorship: pallet_authorship,
		AssetTxPayment: pallet_asset_tx_payment,
	}
);

parameter_types! {
	pub(crate) static ExtrinsicBaseWeight: Weight = Weight::zero();
}

pub struct BlockWeights;
impl Get<frame_system::limits::BlockWeights> for BlockWeights {
	fn get() -> frame_system::limits::BlockWeights {
		frame_system::limits::BlockWeights::builder()
			.base_block(Weight::zero())
			.for_class(DispatchClass::all(), |weights| {
				weights.base_extrinsic = ExtrinsicBaseWeight::get().into();
			})
			.for_class(DispatchClass::non_mandatory(), |weights| {
				weights.max_total = Weight::from_parts(1024, u64::MAX).into();
			})
			.build_or_panic()
	}
}

parameter_types! {
	pub static WeightToFee: u64 = 1;
	pub static TransactionByteFee: u64 = 1;
}

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Runtime {
	type BlockWeights = BlockWeights;
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u64>;
}

parameter_types! {
	pub const ExistentialDeposit: u64 = 10;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Runtime {
	type ExistentialDeposit = ConstU64<10>;
	type AccountStore = System;
}

impl WeightToFeeT for WeightToFee {
	type Balance = u64;

	fn weight_to_fee(weight: &Weight) -> Self::Balance {
		Self::Balance::saturated_from(weight.ref_time())
			.saturating_mul(WEIGHT_TO_FEE.with(|v| *v.borrow()))
	}
}

impl WeightToFeeT for TransactionByteFee {
	type Balance = u64;

	fn weight_to_fee(weight: &Weight) -> Self::Balance {
		Self::Balance::saturated_from(weight.ref_time())
			.saturating_mul(TRANSACTION_BYTE_FEE.with(|v| *v.borrow()))
	}
}

pub struct MockTxPaymentWeights;

impl pallet_transaction_payment::WeightInfo for MockTxPaymentWeights {
	fn charge_transaction_payment() -> Weight {
		Weight::from_parts(10, 0)
	}
}

#[derive_impl(pallet_transaction_payment::config_preludes::TestDefaultConfig)]
impl pallet_transaction_payment::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type OnChargeTransaction = FungibleAdapter<Balances, ()>;
	type WeightToFee = WeightToFee;
	type LengthToFee = TransactionByteFee;
	type OperationalFeeMultiplier = ConstU8<5>;
	type WeightInfo = MockTxPaymentWeights;
}

type AssetId = u32;

impl pallet_assets::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Balance = Balance;
	type AssetId = AssetId;
	type AssetIdParameter = codec::Compact<AssetId>;
	type ReserveData = ();
	type Currency = Balances;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<AccountId>>;
	type ForceOrigin = EnsureRoot<AccountId>;
	type AssetDeposit = ConstU64<2>;
	type AssetAccountDeposit = ConstU64<2>;
	type MetadataDepositBase = ConstU64<0>;
	type MetadataDepositPerByte = ConstU64<0>;
	type ApprovalDeposit = ConstU64<0>;
	type StringLimit = ConstU32<20>;
	type Holder = ();
	type Freezer = ();
	type Extra = ();
	type CallbackHandle = ();
	type AssetIdAllocator = ();
	type WeightInfo = ();
	type RemoveItemsLimit = ConstU32<1000>;
	pallet_assets::runtime_benchmarks_enabled! {
		type BenchmarkHelper = ();
	}
}

pub struct HardcodedAuthor;
pub(crate) const BLOCK_AUTHOR: AccountId = 1234;
impl FindAuthor<AccountId> for HardcodedAuthor {
	fn find_author<'a, I>(_: I) -> Option<AccountId>
	where
		I: 'a + IntoIterator<Item = (ConsensusEngineId, &'a [u8])>,
	{
		Some(BLOCK_AUTHOR)
	}
}

impl pallet_authorship::Config for Runtime {
	type FindAuthor = HardcodedAuthor;
	type EventHandler = ();
}

pub struct CreditToBlockAuthor;
impl HandleCredit<AccountId, Assets> for CreditToBlockAuthor {
	fn handle_credit(credit: Credit<AccountId, Assets>) {
		if let Some(author) = pallet_authorship::Pallet::<Runtime>::author() {
			// What to do in case paying the author fails (e.g. because `fee < min_balance`)
			// default: drop the result which will trigger the `OnDrop` of the imbalance.
			let _ = <Assets as Balanced<AccountId>>::resolve(&author, credit);
		}
	}
}

/// Weights used in testing.
pub struct MockWeights;

impl WeightInfo for MockWeights {
	fn charge_asset_tx_payment_zero() -> Weight {
		Weight::from_parts(0, 0)
	}

	fn charge_asset_tx_payment_native() -> Weight {
		Weight::from_parts(15, 0)
	}

	fn charge_asset_tx_payment_asset() -> Weight {
		Weight::from_parts(20, 0)
	}
}

impl Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Fungibles = Assets;
	type OnChargeAssetTransaction = FungiblesAdapter<
		pallet_assets::BalanceToAssetBalance<Balances, Runtime, ConvertInto>,
		CreditToBlockAuthor,
	>;
	type WeightInfo = MockWeights;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = Helper;
}

#[cfg(feature = "runtime-benchmarks")]
pub fn new_test_ext() -> sp_io::TestExternalities {
	let base_weight = 5;
	let balance_factor = 100;
	crate::tests::ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
}

#[cfg(feature = "runtime-benchmarks")]
pub struct Helper;

#[cfg(feature = "runtime-benchmarks")]
impl BenchmarkHelperTrait<u64, u32, u32> for Helper {
	fn create_asset_id_parameter(id: u32) -> (u32, u32) {
		(id.into(), id.into())
	}

	fn setup_balances_and_pool(asset_id: u32, account: u64) {
		use frame_support::{assert_ok, traits::fungibles::Mutate};
		use sp_runtime::traits::StaticLookup;
		let min_balance = 1;
		assert_ok!(Assets::force_create(
			RuntimeOrigin::root(),
			asset_id.into(),
			42,   // owner
			true, // is_sufficient
			min_balance
		));

		// mint into the caller account
		let caller = 2;
		let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
		let balance = 1000;
		assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
		assert_eq!(Assets::balance(asset_id, caller), balance);

		use frame_support::traits::Currency;
		let _ = Balances::deposit_creating(&account, u32::MAX.into());

		let beneficiary = <Runtime as system::Config>::Lookup::unlookup(account);
		let balance = 1000;

		assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
		assert_eq!(Assets::balance(asset_id, account), balance);
	}
}
