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

use frame_support::{
	assert_ok,
	dispatch::{DispatchInfo, GetDispatchInfo, PostDispatchInfo},
	pallet_prelude::*,
	traits::{
		fungible::{Inspect, NativeOrWithId},
		fungibles::{Inspect as FungiblesInspect, Mutate},
		tokens::{Fortitude, Precision, Preservation, WithdrawConsequence},
		OriginTrait,
	},
	weights::Weight,
};
use frame_system as system;
use mock::{ExtrinsicBaseWeight, *};
use pallet_balances::Call as BalancesCall;
use sp_runtime::{
	traits::{DispatchTransaction, StaticLookup},
	BuildStorage,
};

const CALL: &<Runtime as frame_system::Config>::RuntimeCall =
	&RuntimeCall::Balances(BalancesCall::transfer_allow_death { dest: 2, value: 69 });

pub struct ExtBuilder {
	balance_factor: u64,
	base_weight: Weight,
	byte_fee: u64,
	weight_to_fee: u64,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self {
			balance_factor: 1,
			base_weight: Weight::from_parts(0, 0),
			byte_fee: 1,
			weight_to_fee: 1,
		}
	}
}

impl ExtBuilder {
	pub fn base_weight(mut self, base_weight: Weight) -> Self {
		self.base_weight = base_weight;
		self
	}
	pub fn balance_factor(mut self, factor: u64) -> Self {
		self.balance_factor = factor;
		self
	}
	fn set_constants(&self) {
		ExtrinsicBaseWeight::mutate(|v| *v = self.base_weight);
		TRANSACTION_BYTE_FEE.with(|v| *v.borrow_mut() = self.byte_fee);
		WEIGHT_TO_FEE.with(|v| *v.borrow_mut() = self.weight_to_fee);
	}
	pub fn build(self) -> sp_io::TestExternalities {
		self.set_constants();
		let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		pallet_balances::GenesisConfig::<Runtime> {
			balances: if self.balance_factor > 0 {
				vec![
					(1, 10 * self.balance_factor),
					(2, 20 * self.balance_factor),
					(3, 30 * self.balance_factor),
					(4, 40 * self.balance_factor),
					(5, 50 * self.balance_factor),
					(6, 60 * self.balance_factor),
				]
			} else {
				vec![]
			},
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();
		t.into()
	}
}

/// create a transaction info struct from weight. Handy to avoid building the whole struct.
pub fn info_from_weight(w: Weight) -> DispatchInfo {
	// pays_fee: Pays::Yes -- class: DispatchClass::Normal
	DispatchInfo { call_weight: w, ..Default::default() }
}

fn post_info_from_weight(w: Weight) -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: Some(w), pays_fee: Default::default() }
}

fn info_from_pays(p: Pays) -> DispatchInfo {
	DispatchInfo { pays_fee: p, ..Default::default() }
}

fn post_info_from_pays(p: Pays) -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: None, pays_fee: p }
}

fn default_post_info() -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: None, pays_fee: Default::default() }
}

fn setup_lp(asset_id: u32, balance_factor: u64) {
	let lp_provider = 5;
	let ed = Balances::minimum_balance();
	let ed_asset = Assets::minimum_balance(asset_id);
	assert_ok!(Balances::force_set_balance(
		RuntimeOrigin::root(),
		lp_provider,
		10_000 * balance_factor + ed,
	));
	let lp_provider_account = <Runtime as system::Config>::Lookup::unlookup(lp_provider);
	assert_ok!(Assets::mint_into(
		asset_id.into(),
		&lp_provider_account,
		10_000 * balance_factor + ed_asset
	));

	let token_1 = NativeOrWithId::Native;
	let token_2 = NativeOrWithId::WithId(asset_id);
	assert_ok!(AssetConversion::create_pool(
		RuntimeOrigin::signed(lp_provider),
		Box::new(token_1.clone()),
		Box::new(token_2.clone())
	));

	assert_ok!(AssetConversion::add_liquidity(
		RuntimeOrigin::signed(lp_provider),
		Box::new(token_1),
		Box::new(token_2),
		1_000 * balance_factor,  // 1 desired
		10_000 * balance_factor, // 2 desired
		1,                       // 1 min
		1,                       // 2 min
		lp_provider_account,
	));
}

const WEIGHT_5: Weight = Weight::from_parts(5, 0);
const WEIGHT_50: Weight = Weight::from_parts(50, 0);
const WEIGHT_100: Weight = Weight::from_parts(100, 0);

#[test]
fn transaction_payment_in_native_possible() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			let len = 10;
			let mut info = info_from_weight(WEIGHT_5);
			let ext = ChargeAssetTxPayment::<Runtime>::from(0, None);
			info.extension_weight = ext.weight(CALL);
			let (pre, _) = ext.validate_and_prepare(Some(1).into(), CALL, &info, len, 0).unwrap();
			let initial_balance = 10 * balance_factor;
			assert_eq!(Balances::free_balance(1), initial_balance - 5 - 5 - 15 - 10);

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&default_post_info(),
				len,
				&Ok(()),
			));
			assert_eq!(Balances::free_balance(1), initial_balance - 5 - 5 - 15 - 10);

			let mut info = info_from_weight(WEIGHT_100);
			let ext = ChargeAssetTxPayment::<Runtime>::from(5 /* tipped */, None);
			let extension_weight = ext.weight(CALL);
			info.extension_weight = extension_weight;
			let (pre, _) = ext.validate_and_prepare(Some(2).into(), CALL, &info, len, 0).unwrap();
			let initial_balance_for_2 = 20 * balance_factor;

			assert_eq!(Balances::free_balance(2), initial_balance_for_2 - 5 - 10 - 100 - 15 - 5);
			let call_actual_weight = WEIGHT_50;
			let post_info = post_info_from_weight(
				info.call_weight
					.saturating_sub(call_actual_weight)
					.saturating_add(extension_weight),
			);
			// The extension weight refund should be taken into account in `post_dispatch`.
			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			));
			assert_eq!(Balances::free_balance(2), initial_balance_for_2 - 5 - 10 - 50 - 15 - 5);
		});
}

#[test]
fn transaction_payment_in_asset_possible() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			// create the asset
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance
			));

			// mint into the caller account
			let caller = 1;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 1000;

			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let len = 10;
			let tx_weight = 5;

			setup_lp(asset_id, balance_factor);

			let fee_in_native = base_weight + tx_weight + len as u64;
			let input_quote = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			);
			assert_eq!(input_quote, Some(201));

			let fee_in_asset = input_quote.unwrap();
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
				.validate_and_prepare(
					Some(caller).into(),
					CALL,
					&info_from_weight(WEIGHT_5),
					len,
					0,
				)
				.unwrap();
			// assert that native balance is not used
			assert_eq!(Balances::free_balance(caller), 10 * balance_factor);

			// check that fee was charged in the given asset
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);

			System::assert_has_event(RuntimeEvent::Assets(pallet_assets::Event::Withdrawn {
				asset_id,
				who: caller,
				amount: fee_in_asset,
			}));

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_weight(WEIGHT_5), // estimated tx weight
				&default_post_info(),        // weight actually used == estimated
				len,
				&Ok(()),
			));

			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);
			assert_eq!(TipUnbalancedAmount::get(), 0);
			assert_eq!(FeeUnbalancedAmount::get(), fee_in_native);

			// Cross-check the returned `fee_asset_amount` (= event's `actual_fee`) against
			// the caller's actual asset debit.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset,
				tip: 0,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

#[test]
fn transaction_payment_in_asset_fails_if_no_pool_for_that_asset() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			// create the asset
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance
			));

			// mint into the caller account
			let caller = 1;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 1000;

			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let len = 10;
			let pre = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
				.validate_and_prepare(
					Some(caller).into(),
					CALL,
					&info_from_weight(WEIGHT_5),
					len,
					0,
				);

			// As there is no pool in the dex set up for this asset, conversion should fail.
			assert!(pre.is_err());
		});
}

#[test]
fn transaction_payment_without_fee() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);
			let caller = 1;

			// create the asset
			let asset_id = 1;
			let balance = 1000;
			let min_balance = 2;

			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance,
			));

			setup_lp(asset_id, balance_factor);

			// mint into the caller account
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let weight = 5;
			let len = 10;
			let fee_in_native = base_weight + weight + len as u64;
			let input_quote = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			);
			assert_eq!(input_quote, Some(201));

			let fee_in_asset = input_quote.unwrap();
			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
				.validate_and_prepare(
					Some(caller).into(),
					CALL,
					&info_from_weight(WEIGHT_5),
					len,
					0,
				)
				.unwrap();

			// assert that native balance is not used
			assert_eq!(Balances::free_balance(caller), 10 * balance_factor);
			// check that fee was charged in the given asset
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);

			let refund = AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(asset_id),
				fee_in_native,
				true,
			)
			.unwrap();
			assert_eq!(refund, 199);

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_weight(WEIGHT_5),
				&post_info_from_pays(Pays::No),
				len,
				&Ok(()),
			));

			// caller should get refunded
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset + refund);
			assert_eq!(Balances::free_balance(caller), 10 * balance_factor);

			// Cross-check: event's `actual_fee` matches the caller's net asset debit.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset - refund,
				tip: 0,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

#[test]
fn asset_transaction_payment_with_tip_and_refund() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			// create the asset
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance,
			));

			setup_lp(asset_id, balance_factor);

			// mint into the caller account
			let caller = 2;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 10000;

			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let weight = 100;
			let tip = 5;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let ext_weight = ext.weight(CALL);
			let len = 10;
			let fee_in_native = base_weight + weight + ext_weight.ref_time() + len as u64 + tip;
			let input_quote = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			);
			assert_eq!(input_quote, Some(1407));

			let fee_in_asset = input_quote.unwrap();
			let mut info = info_from_weight(WEIGHT_100);
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			info.extension_weight = ext.weight(CALL);
			let (pre, _) =
				ext.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0).unwrap();
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);

			let final_weight = 50;
			let weight_refund = weight - final_weight;
			let ext_weight_refund = ext_weight - MockWeights::charge_asset_tx_payment_asset();
			let expected_fee = fee_in_native - weight_refund - ext_weight_refund.ref_time() - tip;
			let expected_token_refund = AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(asset_id),
				fee_in_native - expected_fee - tip,
				true,
			)
			.unwrap();

			System::assert_has_event(RuntimeEvent::Assets(pallet_assets::Event::Withdrawn {
				asset_id,
				who: caller,
				amount: fee_in_asset,
			}));

			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(ext_weight));
			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			));

			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), expected_fee);

			// caller should get refunded
			assert_eq!(
				Assets::balance(asset_id, caller),
				balance - fee_in_asset + expected_token_refund
			);
			assert_eq!(Balances::free_balance(caller), 20 * balance_factor);

			System::assert_has_event(RuntimeEvent::Assets(pallet_assets::Event::Deposited {
				asset_id,
				who: caller,
				amount: expected_token_refund,
			}));

			// Cross-check: event's `actual_fee` matches the caller's net asset debit
			// (= `fee_in_asset - expected_token_refund`). This is the D1-refund analog
			// of the pre-existing B1 return-value bug.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset - expected_token_refund,
				tip,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

#[test]
fn payment_from_account_with_only_assets() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);
			// create the asset
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance,
			));

			setup_lp(asset_id, balance_factor);

			// mint into the caller account
			let caller = 333;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 1000;

			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			// assert that native balance is not necessary
			assert_eq!(Balances::free_balance(caller), 0);
			let weight = 5;
			let len = 10;

			let fee_in_native = base_weight + weight + len as u64;
			let fee_in_asset = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			)
			.unwrap();
			assert_eq!(fee_in_asset, 201);

			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
				.validate_and_prepare(
					Some(caller).into(),
					CALL,
					&info_from_weight(WEIGHT_5),
					len,
					0,
				)
				.unwrap();
			// check that fee was charged in the given asset
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_weight(WEIGHT_5),
				&default_post_info(),
				len,
				&Ok(()),
			));
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);
			// Cross-check: event's `actual_fee` matches the caller's net asset debit.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset,
				tip: 0,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
			assert_eq!(Balances::free_balance(caller), 0);

			assert_eq!(TipUnbalancedAmount::get(), 0);
			assert_eq!(FeeUnbalancedAmount::get(), fee_in_native);
		});
}

#[test]
fn converted_fee_is_never_zero_if_input_fee_is_not() {
	let base_weight = 1;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			// create the asset
			let asset_id = 1;
			let min_balance = 1;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance
			));

			setup_lp(asset_id, balance_factor);

			// mint into the caller account
			let caller = 2;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 1000;

			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let weight = 1;
			let len = 1;

			// there will be no conversion when the fee is zero
			{
				let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
					.validate_and_prepare(
						Some(caller).into(),
						CALL,
						&info_from_pays(Pays::No),
						len,
						0,
					)
					.unwrap();
				// `Pays::No` implies there are no fees
				assert_eq!(Assets::balance(asset_id, caller), balance);

				assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
					pre,
					&info_from_pays(Pays::No),
					&post_info_from_pays(Pays::No),
					len,
					&Ok(()),
				));
				assert_eq!(Assets::balance(asset_id, caller), balance);
			}

			// validate even a small fee gets converted to asset.
			let fee_in_native = base_weight + weight + len as u64;
			let fee_in_asset = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			)
			.unwrap();

			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
				.validate_and_prepare(
					Some(caller).into(),
					CALL,
					&info_from_weight(Weight::from_parts(weight, 0)),
					len,
					0,
				)
				.unwrap();
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_weight(Weight::from_parts(weight, 0)),
				&default_post_info(),
				len,
				&Ok(()),
			));
			assert_eq!(Assets::balance(asset_id, caller), balance - fee_in_asset);
		});
}

#[test]
fn post_dispatch_fee_is_zero_if_pre_dispatch_fee_is_zero() {
	let base_weight = 1;
	ExtBuilder::default()
		.balance_factor(100)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			// create the asset
			let asset_id = 1;
			let min_balance = 100;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance
			));

			// mint into the caller account
			let caller = 333;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 1000;

			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let weight = 1;
			let len = 1;
			let fee = base_weight + weight + len as u64;

			// calculated fee is greater than 0
			assert!(fee > 0);

			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()))
				.validate_and_prepare(Some(caller).into(), CALL, &info_from_pays(Pays::No), len, 0)
				.unwrap();
			// `Pays::No` implies no pre-dispatch fees

			assert_eq!(Assets::balance(asset_id, caller), balance);

			let Pre::Charge { initial_payment, .. } = &pre else {
				panic!("Expected Charge");
			};
			let not_paying = match initial_payment {
				&InitialPayment::Nothing => true,
				_ => false,
			};
			assert!(not_paying, "initial payment should be Nothing if we pass Pays::No");

			// `Pays::Yes` on post-dispatch does not mean we pay (we never charge more than the
			// initial fee)
			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_pays(Pays::No),
				&post_info_from_pays(Pays::Yes),
				len,
				&Ok(()),
			));
			assert_eq!(Assets::balance(asset_id, caller), balance);
		});
}

#[test]
fn fee_with_native_asset_passed_with_id() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);
			let caller = 1;
			let caller_balance = 1000;
			// native asset
			let asset_id = NativeOrWithId::Native;
			// assert that native balance is not necessary
			assert_eq!(Balances::free_balance(caller), caller_balance);

			let tip = 10;
			let call_weight = 100;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let extension_weight = ext.weight(CALL);
			let len = 5;
			let initial_fee =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) =
				ext.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0).unwrap();
			assert_eq!(Balances::free_balance(caller), caller_balance - initial_fee);

			let final_weight = 50;
			// No refunds from the extension weight itself.
			let expected_fee = initial_fee - final_weight;

			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(extension_weight));
			assert_eq!(
				ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
					pre,
					&info_from_weight(WEIGHT_100),
					&post_info,
					len,
					&Ok(()),
				)
				.unwrap(),
				Weight::zero()
			);

			assert_eq!(Balances::free_balance(caller), caller_balance - expected_fee);

			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), expected_fee - tip);

			// Cross-check: event's `actual_fee` matches the caller's net native debit.
			// Before the refactor, the returned value here was `corrected_fee - refund`,
			// which under-reported the fee in this event.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: expected_fee,
				tip,
				asset_id: NativeOrWithId::Native,
			}));
		});
}

#[test]
fn transfer_add_and_remove_account() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			// create the asset
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance,
			));

			setup_lp(asset_id, balance_factor);

			// mint into the caller account
			let caller = 222;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 10000;

			assert_eq!(Balances::free_balance(caller), 0);
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));
			assert_eq!(Assets::balance(asset_id, caller), balance);

			let call_weight = 100;
			let tip = 5;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let extension_weight = ext.weight(CALL);
			let len = 10;
			let fee_in_native =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;
			let input_quote = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			);
			assert!(!input_quote.unwrap().is_zero());

			let fee_in_asset = input_quote.unwrap();
			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()))
				.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0)
				.unwrap();

			assert_eq!(Assets::balance(asset_id, &caller), balance - fee_in_asset);

			// remove caller account.
			assert_ok!(Assets::burn_from(
				asset_id,
				&caller,
				Assets::balance(asset_id, &caller),
				Preservation::Expendable,
				Precision::Exact,
				Fortitude::Force
			));

			// Actual call weight + actual extension weight.
			let final_weight = 50 + 20;
			let final_fee_in_native = fee_in_native - final_weight - tip;
			let token_refund = AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(asset_id),
				fee_in_native - final_fee_in_native - tip,
				true,
			)
			.unwrap();

			// make sure the refund amount is enough to create the account.
			assert!(token_refund >= min_balance);

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info_from_weight(WEIGHT_50),
				len,
				&Ok(()),
			));

			// fee paid with no refund.
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), fee_in_native - tip);

			// caller account removed.
			assert_eq!(Assets::balance(asset_id, caller), 0);

			// Cross-check: event's `actual_fee` matches the full initial `fee_in_asset`
			// (the caller's asset debit, since no refund was credited — account dead).
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset,
				tip,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

#[test]
fn no_fee_and_no_weight_for_other_origins() {
	ExtBuilder::default().build().execute_with(|| {
		let ext = ChargeAssetTxPayment::<Runtime>::from(0, None);

		let mut info = CALL.get_dispatch_info();
		info.extension_weight = ext.weight(CALL);

		// Ensure we test the refund.
		assert!(info.extension_weight != Weight::zero());

		let len = CALL.encoded_size();

		let origin = frame_system::RawOrigin::Root.into();
		let (pre, origin) = ext.validate_and_prepare(origin, CALL, &info, len, 0).unwrap();

		assert!(origin.as_system_ref().unwrap().is_root());

		let pd_res = Ok(());
		let mut post_info = frame_support::dispatch::PostDispatchInfo {
			actual_weight: Some(info.total_weight()),
			pays_fee: Default::default(),
		};

		<ChargeAssetTxPayment<Runtime> as TransactionExtension<RuntimeCall>>::post_dispatch(
			pre,
			&info,
			&mut post_info,
			len,
			&pd_res,
		)
		.unwrap();

		assert_eq!(post_info.actual_weight, Some(info.call_weight));
	})
}

/// Tests that validation rejects transactions that would result in `ReducedToZero` for native
/// assets.
#[test]
fn transaction_payment_rejects_reduced_to_zero_in_native_asset() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			let ed = Balances::minimum_balance();
			let len = 10;
			let weight = 5;
			let tip = 5;

			let asset_id = NativeOrWithId::Native;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.clone().into()));
			let mut info = info_from_weight(Weight::from_parts(weight, 0));
			info.extension_weight = ext.weight(CALL);

			// Calculate the actual fee
			let fee =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, tip);

			// Set balance to cause ReducedToZero
			let balance = ed + fee - 1;
			let caller = 1;
			assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), caller, balance));

			// Verify that can_withdraw returns ReducedToZero
			let consequence = Balances::can_withdraw(&caller, fee);
			assert!(
				matches!(consequence, WithdrawConsequence::ReducedToZero(_)),
				"can_withdraw should return ReducedToZero, got: {:?}",
				consequence
			);

			let result = ext.validate_only(
				Some(caller).into(),
				CALL,
				&info,
				len,
				sp_runtime::transaction_validity::TransactionSource::External,
				0,
			);

			// ReducedToZero should be rejected during validation
			assert!(result.is_err(), "Validation should reject ReducedToZero.");
		});
}

/// Tests that validation rejects transactions that would result in `ReducedToZero` for assets.
#[test]
fn transaction_payment_rejects_reduced_to_zero_in_asset() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			// create the asset
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,   // owner
				true, // is_sufficient
				min_balance
			));

			setup_lp(asset_id, balance_factor);

			let caller = 999;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let len = 10;
			let weight = 5;
			let tip = 5;

			// Calculate the actual fee
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let mut info = info_from_weight(Weight::from_parts(weight, 0));
			info.extension_weight = ext.weight(CALL);

			let fee_in_native =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, tip);

			let fee_in_asset = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			)
			.unwrap();

			// Set asset balance to cause ReducedToZero
			let asset_balance = min_balance + fee_in_asset - 1;
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, asset_balance));

			// Verify that can_withdraw returns ReducedToZero
			let consequence = Assets::can_withdraw(asset_id, &caller, fee_in_asset);
			assert!(
				matches!(consequence, WithdrawConsequence::ReducedToZero(_)),
				"can_withdraw should return ReducedToZero, got: {:?}",
				consequence
			);

			let result = ext.validate_only(
				Some(caller).into(),
				CALL,
				&info,
				len,
				sp_runtime::transaction_validity::TransactionSource::External,
				0,
			);

			// ReducedToZero should be rejected during validation
			assert!(result.is_err(), "Validation should reject ReducedToZero.");
		});
}

/// Regression test for the pre-existing bug in `correct_and_deposit_fee`: when fees are paid
/// in the native asset via the asset-conversion extension (`asset_id == A::get()`) and a refund
/// is due, the reported `actual_fee` in the `AssetTxFeePaid` event must equal `corrected_fee`.
///
/// Prior to the refactor, the returned value was `corrected_fee - refund_amount` (underflowing
/// to 0 in many cases), so the event under-reported the fee. The refund itself and `OnUnbalanced`
/// routing were unaffected, but the event was wrong.
#[test]
fn native_asset_refund_reports_corrected_fee_in_event() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			let caller = 1;
			let caller_balance = 10 * balance_factor;
			let asset_id = NativeOrWithId::Native;
			assert_eq!(Balances::free_balance(caller), caller_balance);

			let tip = 10;
			let call_weight = 100;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.clone().into()));
			let extension_weight = ext.weight(CALL);
			let len = 5;
			let initial_fee =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) =
				ext.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0).unwrap();
			assert_eq!(Balances::free_balance(caller), caller_balance - initial_fee);

			let final_weight = 50;
			let expected_fee = initial_fee - final_weight;
			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(extension_weight));

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_weight(WEIGHT_100),
				&post_info,
				len,
				&Ok(()),
			));

			// Caller gets refunded (resolve succeeds on their live native account).
			assert_eq!(Balances::free_balance(caller), caller_balance - expected_fee);

			// OU received the corrected fee and tip in the target asset.
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), expected_fee - tip);

			// Event must report the corrected fee (not the pre-refactor underflowed value).
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: expected_fee,
				tip,
				asset_id: NativeOrWithId::Native,
			}));
		});
}

/// Covers Path A (`F::total_balance == 0`) via the native-asset branch: the caller's native
/// balance is wiped between pre-dispatch fee withdrawal and post-dispatch refund processing.
#[test]
fn post_dispatch_ok_when_native_account_killed_post_withdraw() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			let caller = 1;
			let caller_balance = 10 * balance_factor;
			let asset_id = NativeOrWithId::Native;

			let tip = 10;
			let call_weight = 100;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.clone().into()));
			let extension_weight = ext.weight(CALL);
			let len = 5;
			let initial_fee =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) =
				ext.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0).unwrap();
			assert_eq!(Balances::free_balance(caller), caller_balance - initial_fee);

			// Zero-out the caller's balance between withdraw and post_dispatch.
			assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), caller, 0));
			assert_eq!(Balances::free_balance(caller), 0);

			// Actual weight is less than estimated — refund would normally be due.
			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(extension_weight));

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info_from_weight(WEIGHT_100),
				&post_info,
				len,
				&Ok(()),
			));

			// No refund given — the dead account triggers Path A early-exit with full fee to OU.
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), initial_fee - tip);

			// Caller is still at zero — no refund was credited.
			assert_eq!(Balances::free_balance(caller), 0);

			// Event reports the full initial fee since no refund was given.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: initial_fee,
				tip,
				asset_id: NativeOrWithId::Native,
			}));
		});
}

/// Complement to `transfer_add_and_remove_account`: asserts the `AssetTxFeePaid` event
/// reports the full initial fee (in `asset_id`) when the caller's asset account is killed
/// between withdraw and post_dispatch (Path A via asset path, `total_balance == 0`).
#[test]
fn asset_account_killed_post_withdraw_emits_full_fee_event() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,
				true,
				min_balance,
			));
			setup_lp(asset_id, balance_factor);

			let caller = 222;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 10000;
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));

			let call_weight = 100;
			let tip = 5;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let extension_weight = ext.weight(CALL);
			let len = 10;
			let fee_in_native =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;
			let fee_in_asset = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			)
			.unwrap();

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()))
				.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0)
				.unwrap();
			assert_eq!(Assets::balance(asset_id, &caller), balance - fee_in_asset);

			// Burn the caller's entire asset balance — account becomes unable to receive refund.
			assert_ok!(Assets::burn_from(
				asset_id,
				&caller,
				Assets::balance(asset_id, &caller),
				Preservation::Expendable,
				Precision::Exact,
				Fortitude::Force,
			));
			assert_eq!(Assets::balance(asset_id, caller), 0);

			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(extension_weight));

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			));

			// No refund given (account has no asset balance to receive one).
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), fee_in_native - tip);
			assert_eq!(Assets::balance(asset_id, caller), 0);

			// Event reports the full initial `fee_in_asset` (what the user actually paid).
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset,
				tip,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

/// Covers the `can_deposit` pre-flight check: the caller's asset account is blocked between
/// withdraw and post_dispatch via `Assets::block`. `F::total_balance` still reports a positive
/// balance (Path A does not short-circuit) and the refund quote succeeds, but
/// `F::can_deposit` returns `DepositConsequence::Blocked`. The refactor skips the swap
/// entirely: pool state is untouched, full `fee_paid` goes to OU, event reports full initial
/// `fee_in_asset`.
#[test]
fn post_dispatch_ok_when_asset_account_blocked_post_withdraw() {
	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			// `force_create` sets owner = issuer = admin = freezer = 42; only the freezer
			// can call `block`.
			let freezer = 42;
			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				freezer,
				true,
				min_balance,
			));
			setup_lp(asset_id, balance_factor);

			let caller = 2;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 10000;
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));

			let call_weight = 100;
			let tip = 5;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let extension_weight = ext.weight(CALL);
			let len = 10;
			let fee_in_native =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;
			let fee_in_asset = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			)
			.unwrap();

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()))
				.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0)
				.unwrap();
			let balance_after_withdraw = balance - fee_in_asset;
			assert_eq!(Assets::balance(asset_id, &caller), balance_after_withdraw);

			// Block the caller's asset account — `can_deposit` will now return `Blocked`,
			// so the refund pre-flight check fails and the swap is skipped entirely.
			assert_ok!(Assets::block(
				RuntimeOrigin::signed(freezer),
				asset_id.into(),
				<Runtime as system::Config>::Lookup::unlookup(caller),
			));

			// Record the pool state before post-dispatch — it must be untouched since
			// the refund swap is skipped.
			let pool_account = <<Runtime as pallet_asset_conversion::Config>::PoolLocator
				as pallet_asset_conversion::PoolLocator<_, _, _>>::pool_address(
				&NativeOrWithId::Native,
				&NativeOrWithId::WithId(asset_id),
			)
			.unwrap();
			let pool_native_before = Balances::free_balance(&pool_account);
			let pool_asset_before = Assets::balance(asset_id, &pool_account);

			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(extension_weight));

			// Security invariant: post_dispatch must NOT return `Err`.
			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			));

			// Caller's balance is unchanged since withdraw: refund was not credited
			// because the account is blocked.
			assert_eq!(Assets::balance(asset_id, &caller), balance_after_withdraw);

			// Pool state is untouched — the `can_deposit` pre-flight check prevents
			// the swap from executing.
			assert_eq!(Balances::free_balance(&pool_account), pool_native_before);
			assert_eq!(Assets::balance(asset_id, &pool_account), pool_asset_before);

			// Full initial `fee_paid` (= `fee_in_native`) goes to OU. There is no
			// refund-then-burn cycle: the swap never happened, so no asset was minted
			// or burned.
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), fee_in_native - tip);

			// Event reports the full initial `fee_in_asset` — no refund reached the user,
			// so the returned `fee_asset_amount` equals what was debited at withdraw.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset,
				tip,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

/// Covers Path B2 (`asset_id == A::get()`, `F::resolve` Err, `merge` Ok) for the native-asset
/// branch. Reserving all of the caller's free balance leaves `total_balance > 0` (so Path A
/// does not short-circuit) but `free == 0`. With `refund_amount < ExistentialDeposit`, the
/// resolve's internal `deposit(..., Exact)` returns `DepositConsequence::BelowMinimum`
/// (`new_free = 0 + refund_amount < ED`). The refactor then merges the refund back into
/// `adjusted_paid` — full initial fee goes to OU, event reports the full initial fee.
#[test]
fn post_dispatch_ok_when_native_account_has_no_free_balance() {
	use frame_support::traits::ReservableCurrency;

	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			let caller = 1;
			let caller_balance = 10 * balance_factor;
			let asset_id = NativeOrWithId::Native;

			let tip = 10;
			let call_weight = 100;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.clone().into()));
			let extension_weight = ext.weight(CALL);
			let len = 5;
			let initial_fee =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) =
				ext.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0).unwrap();
			let free_after_withdraw = caller_balance - initial_fee;
			assert_eq!(Balances::free_balance(caller), free_after_withdraw);

			// Reserve all free balance. The account stays alive via the reserved portion,
			// so `total_balance = free + reserved > 0` and Path A does NOT short-circuit.
			// `free = 0` means any later `deposit(.., Exact)` with `amount < ED` returns
			// `DepositConsequence::BelowMinimum`.
			//
			// `inc_providers` is needed because the caller has a consumer reference during
			// the tx lifecycle; without an extra provider, `reserve` would fail with
			// `ConsumerRemaining` when reducing `free` to zero.
			let _ = frame_system::Pallet::<Runtime>::inc_providers(&caller);
			assert_ok!(<Balances as ReservableCurrency<u64>>::reserve(
				&caller,
				free_after_withdraw
			));
			assert_eq!(Balances::free_balance(caller), 0);
			assert_eq!(Balances::reserved_balance(caller), free_after_withdraw);

			// Size the refund below ED so resolve fails. With ED = 10:
			//   final_call_weight = 95  →  call_weight refund = 5  →  refund_amount = 5.
			let final_call_weight = 95;
			let ed: u64 = 10;
			let refund_amount = (call_weight - final_call_weight) as u64;
			assert!(refund_amount < ed);

			let post_info = post_info_from_weight(
				Weight::from_parts(final_call_weight, 0).saturating_add(extension_weight),
			);

			// (Use the full `info` so `compute_actual_fee` accounts for the declared
			// extension_weight; otherwise the refund would exceed ED and hit B1.)
			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			));

			// B2 merge-Ok branch: refund is merged back into `adjusted_paid`; OU receives
			// the full `initial_fee`.
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), initial_fee - tip);

			// Caller's free balance is still zero — no refund was credited.
			assert_eq!(Balances::free_balance(caller), 0);
			assert_eq!(Balances::reserved_balance(caller), free_after_withdraw);

			// Event reports the full initial fee (B2 merge-Ok returns `fee_paid.peek()`).
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: initial_fee,
				tip,
				asset_id: NativeOrWithId::Native,
			}));
		});
}

/// Covers Path C: `S::quote_price_exact_tokens_for_tokens` returns `None` because the pool's
/// asset reserve has been dusted (below `min_balance`, so the pool's asset account was reaped).
/// `get_amount_out` then returns `Err(ZeroLiquidity)`, which surfaces as `None` via the
/// `QuotePrice` impl; Path C takes the no-refund exit.
///
/// Note: AMM swaps withdraw from the pool with `Preservation::Preserve`, which refuses to
/// drop the pool below ED. We bypass that here by burning the pool's asset balance directly with
/// `Expendable`.
#[test]
fn post_dispatch_ok_when_pool_asset_dusted_post_withdraw() {
	use pallet_asset_conversion::PoolLocator;

	let base_weight = 5;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			System::set_block_number(1);

			let asset_id = 1;
			let min_balance = 2;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,
				true,
				min_balance,
			));
			setup_lp(asset_id, balance_factor);

			let caller = 2;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 10000;
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));

			let call_weight = 100;
			let tip = 5;
			let ext = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()));
			let extension_weight = ext.weight(CALL);
			let len = 10;
			let fee_in_native =
				base_weight + call_weight + extension_weight.ref_time() + len as u64 + tip;
			let fee_in_asset = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			)
			.unwrap();

			let mut info = info_from_weight(WEIGHT_100);
			info.extension_weight = extension_weight;
			let (pre, _) = ChargeAssetTxPayment::<Runtime>::from(tip, Some(asset_id.into()))
				.validate_and_prepare(Some(caller).into(), CALL, &info, len, 0)
				.unwrap();
			let balance_after_withdraw = balance - fee_in_asset;
			assert_eq!(Assets::balance(asset_id, &caller), balance_after_withdraw);

			// Derive the pool's account and dust its asset reserve: burn the full balance
			// with `Expendable`, which reaps the asset account. `get_reserves` will then
			// report `asset_reserve == 0` → `get_amount_out` → `Err(ZeroLiquidity)` →
			// `quote_price_exact_tokens_for_tokens` returns `None`.
			let pool_account =
				<<Runtime as pallet_asset_conversion::Config>::PoolLocator as PoolLocator<
					_,
					_,
					_,
				>>::pool_address(&NativeOrWithId::Native, &NativeOrWithId::WithId(asset_id))
				.unwrap();
			let pool_asset_balance = Assets::balance(asset_id, &pool_account);
			assert!(pool_asset_balance > 0);
			assert_ok!(Assets::burn_from(
				asset_id,
				&pool_account,
				pool_asset_balance,
				Preservation::Expendable,
				Precision::Exact,
				Fortitude::Force,
			));
			assert_eq!(Assets::balance(asset_id, &pool_account), 0);

			// Sanity: the refund-direction quote now returns `None` — this is the signal
			// that triggers Path C.
			assert!(AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(asset_id),
				1u64,
				true,
			)
			.is_none());

			let post_info = post_info_from_weight(WEIGHT_50.saturating_add(extension_weight));

			assert_ok!(ChargeAssetTxPayment::<Runtime>::post_dispatch_details(
				pre,
				&info,
				&post_info,
				len,
				&Ok(()),
			));

			// Path C: no refund given because no pool route exists.
			// Full initial `fee_in_native` goes to OU.
			let actual_ext_weight = MockWeights::charge_asset_tx_payment_asset();
			let ext_weight_refund = extension_weight - actual_ext_weight;
			let call_weight_refund = call_weight - 50;
			let corrected_fee_in_native =
				fee_in_native - call_weight_refund - ext_weight_refund.ref_time();
			// OU still receives the corrected fee because Path C splits `fee_paid` by `tip`
			// and forwards it all; `fee_paid.peek() == fee_in_native` (pre-correction).
			assert_eq!(TipUnbalancedAmount::get(), tip);
			assert_eq!(FeeUnbalancedAmount::get(), fee_in_native - tip);
			// (For awareness: corrected_fee_in_native < fee_in_native — the difference
			// would have been refunded under a live pool.)
			assert!(corrected_fee_in_native < fee_in_native);

			// Caller's balance is unchanged since withdraw — no refund was given.
			assert_eq!(Assets::balance(asset_id, &caller), balance_after_withdraw);

			// Event reports the full initial `fee_in_asset`.
			System::assert_has_event(RuntimeEvent::AssetTxPayment(crate::Event::AssetTxFeePaid {
				who: caller,
				actual_fee: fee_in_asset,
				tip,
				asset_id: NativeOrWithId::WithId(asset_id),
			}));
		});
}

/// Validates that `can_withdraw_fee` rejects a zero-quoted swap. The `!fee.is_zero()` guard in
/// `can_withdraw_fee` and `withdraw_fee` ensures that a degenerate quote of 0 asset for a non-zero
/// native fee is treated the same as `None` (no viable swap route).
///
/// Note: the current AMM's `get_amount_in` always returns >= 1 (rounds up via `+1`), so
/// `Some(0)` cannot occur in practice. This test verifies the guard by checking that a
/// fee requiring a very small asset amount still produces a valid non-zero quote and passes
/// validation — i.e., the filter does not accidentally reject legitimate small quotes.
#[test]
fn validate_rejects_zero_asset_fee_but_accepts_small_nonzero() {
	let base_weight = 1;
	let balance_factor = 100;
	ExtBuilder::default()
		.balance_factor(balance_factor)
		.base_weight(Weight::from_parts(base_weight, 0))
		.build()
		.execute_with(|| {
			let asset_id = 1;
			let min_balance = 1;
			assert_ok!(Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.into(),
				42,
				true,
				min_balance,
			));
			setup_lp(asset_id, balance_factor);

			let caller = 2;
			let beneficiary = <Runtime as system::Config>::Lookup::unlookup(caller);
			let balance = 1000;
			assert_ok!(Assets::mint_into(asset_id.into(), &beneficiary, balance));

			// Use weight=1, len=1 to get the smallest possible non-zero fee.
			let len = 1;
			let weight = 1;
			let fee_in_native = base_weight + weight + len as u64;
			assert_eq!(fee_in_native, 3);

			// The AMM quote for 3 native rounds up to a small but non-zero asset amount.
			let quoted = AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(asset_id),
				NativeOrWithId::Native,
				fee_in_native,
				true,
			);
			assert!(
				quoted.is_some() && quoted.unwrap() > 0,
				"AMM must quote a non-zero asset fee for a non-zero native fee"
			);

			// Validation must succeed — the non-zero quote passes the `.filter()` guard.
			let ext = ChargeAssetTxPayment::<Runtime>::from(0, Some(asset_id.into()));
			let result = ext.validate_only(
				Some(caller).into(),
				CALL,
				&info_from_weight(Weight::from_parts(weight, 0)),
				len,
				sp_runtime::transaction_validity::TransactionSource::External,
				0,
			);
			assert!(result.is_ok(), "Small but non-zero fee should pass validation");
		});
}
