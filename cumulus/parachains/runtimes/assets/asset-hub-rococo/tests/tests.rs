// This file is part of Cumulus.

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

//! Tests for the Rococo Assets Hub chain.

use asset_hub_rococo_runtime::{
	xcm_config,
	xcm_config::{
		bridging, CheckingAccount, GovernanceLocation, LocationToAccountId, StakingPot,
		TokenLocation, TrustBackedAssetsPalletLocation, XcmConfig,
	},
	AllPalletsWithoutSystem, AssetConversion, AssetDeposit, Assets, Balances, Block,
	CollatorSelection, ExistentialDeposit, ForeignAssets, ForeignAssetsInstance,
	MetadataDepositBase, MetadataDepositPerByte, ParachainSystem, Runtime, RuntimeCall,
	RuntimeEvent, RuntimeOrigin, SessionKeys, ToWestendXcmRouterInstance,
	TrustBackedAssetsInstance, XcmpQueue,
};
use asset_test_utils::{
	test_cases_over_bridge::TestBridgingConfig, CollatorSessionKey, CollatorSessionKeys,
	ExtBuilder, GovernanceOrigin, SlotDurations,
};
use codec::{Decode, Encode};
use frame_support::{
	assert_ok, parameter_types,
	traits::{
		fungible::{Inspect, Mutate},
		fungibles::{
			Create, Inspect as FungiblesInspect, InspectEnumerable, Mutate as FungiblesMutate,
		},
	},
	weights::{Weight, WeightToFee as WeightToFeeT},
};
use hex_literal::hex;
use parachains_common::{AccountId, AssetIdForTrustBackedAssets, AuraId, Balance};
use sp_consensus_aura::SlotDuration;
use sp_core::crypto::Ss58Codec;
use sp_runtime::traits::{MaybeEquivalence, TryConvertInto};
use std::convert::Into;
use testnet_parachains_constants::rococo::{consensus::*, currency::UNITS, fee::WeightToFee};
use xcm::latest::{
	prelude::{Assets as XcmAssets, *},
	WESTEND_GENESIS_HASH,
};
use xcm_builder::WithLatestLocationConverter;
use xcm_executor::traits::{TransactAsset, WeightTrader};
use xcm_runtime_apis::conversions::LocationToAccountHelper;

const ALICE: [u8; 32] = [1u8; 32];
const SOME_ASSET_ADMIN: [u8; 32] = [5u8; 32];

parameter_types! {
	pub Governance: GovernanceOrigin<RuntimeOrigin> = GovernanceOrigin::Location(GovernanceLocation::get());
}

/// Helper to convert a single Asset into AssetsInHolding for tests
/// This creates a proper AssetsInHolding by withdrawing from an account
fn asset_to_holding_withdraw(asset: Asset, who: &AccountId) -> xcm_executor::AssetsInHolding {
	use xcm_executor::traits::TransactAsset;
	let who_location: Location =
		Junction::AccountId32 { network: None, id: who.clone().into() }.into();
	<XcmConfig as xcm_executor::Config>::AssetTransactor::withdraw_asset(
		&asset,
		&who_location,
		None,
	)
	.expect("failed to withdraw asset")
}

/// Helper to convert a single Asset into AssetsInHolding for tests (mock version for error tests)
fn asset_to_holding(asset: Asset) -> xcm_executor::AssetsInHolding {
	xcm_executor::test_helpers::mock_asset_to_holding(asset)
}

type AssetIdForTrustBackedAssetsConvert =
	assets_common::AssetIdForTrustBackedAssetsConvert<TrustBackedAssetsPalletLocation>;

type RuntimeHelper = asset_test_utils::RuntimeHelper<Runtime, AllPalletsWithoutSystem>;

fn collator_session_key(account: [u8; 32]) -> CollatorSessionKey<Runtime> {
	CollatorSessionKey::new(
		AccountId::from(account),
		AccountId::from(account),
		SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(account)) },
	)
}

fn collator_session_keys() -> CollatorSessionKeys<Runtime> {
	CollatorSessionKeys::default().add(collator_session_key(ALICE))
}

fn slot_durations() -> SlotDurations {
	SlotDurations {
		relay: SlotDuration::from_millis(RELAY_CHAIN_SLOT_DURATION_MILLIS.into()),
		para: SlotDuration::from_millis(SLOT_DURATION),
	}
}

#[test]
fn test_buy_and_refund_weight_in_native() {
	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let bob: AccountId = SOME_ASSET_ADMIN.into();
			let staking_pot = CollatorSelection::account_id();
			let native_location = TokenLocation::get();
			let initial_balance = 200 * UNITS;

			assert_ok!(Balances::mint_into(&bob, initial_balance));
			assert_ok!(Balances::mint_into(&staking_pot, initial_balance));

			// keep initial total issuance to assert later.
			let total_issuance = Balances::total_issuance();

			// prepare input to buy weight.
			let weight = Weight::from_parts(4_000_000_000, 0);
			let fee = WeightToFee::weight_to_fee(&weight);
			let extra_amount = 100;
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
			let payment: Asset = (native_location.clone(), fee + extra_amount).into();

			// init trader and buy weight.
			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let unused_asset = trader
				.buy_weight(weight, asset_to_holding_withdraw(payment, &bob), &ctx)
				.expect("Expected Ok");

			// assert.
			let unused_amount = unused_asset
				.fungible
				.get(&native_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(unused_amount, extra_amount);
			assert_eq!(Balances::total_issuance(), total_issuance);

			// prepare input to refund weight.
			let refund_weight = Weight::from_parts(1_000_000_000, 0);
			let refund = WeightToFee::weight_to_fee(&refund_weight);

			// refund.
			let actual_refund = trader.refund_weight(refund_weight, &ctx).unwrap();
			let expected_refund = asset_to_holding((native_location, refund).into());
			assert_eq!(actual_refund, expected_refund);

			// assert.
			assert_eq!(Balances::balance(&staking_pot), initial_balance);
			// only after `trader` is dropped we expect the fee to be resolved into the treasury
			// account.
			drop(trader);
			assert_eq!(Balances::balance(&staking_pot), initial_balance + fee - refund);
			assert_eq!(Balances::total_issuance(), total_issuance);
		})
}

#[test]
fn test_buy_and_refund_weight_with_swap_local_asset_xcm_trader() {
	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let bob: AccountId = SOME_ASSET_ADMIN.into();
			let staking_pot = CollatorSelection::account_id();
			let asset_1: u32 = 1;
			let native_location = TokenLocation::get();
			let asset_1_location =
				AssetIdForTrustBackedAssetsConvert::convert_back(&asset_1).unwrap();
			// bob's initial balance for native and `asset1` assets.
			let initial_balance = 200 * UNITS;
			// liquidity for both arms of (native, asset1) pool.
			let pool_liquidity = 100 * UNITS;

			// init asset, balances and pool.
			assert_ok!(<Assets as Create<_>>::create(asset_1, bob.clone(), true, 10));

			assert_ok!(Assets::mint_into(asset_1, &bob, initial_balance));
			assert_ok!(Balances::mint_into(&bob, initial_balance));
			assert_ok!(Balances::mint_into(&staking_pot, initial_balance));

			assert_ok!(AssetConversion::create_pool(
				RuntimeHelper::origin_of(bob.clone()),
				Box::new(Location::try_from(native_location.clone()).expect("conversion works")),
				Box::new(Location::try_from(asset_1_location.clone()).expect("conversion works"))
			));

			assert_ok!(AssetConversion::add_liquidity(
				RuntimeHelper::origin_of(bob.clone()),
				Box::new(Location::try_from(native_location.clone()).expect("conversion works")),
				Box::new(Location::try_from(asset_1_location.clone()).expect("conversion works")),
				pool_liquidity,
				pool_liquidity,
				1,
				1,
				bob.clone(),
			));

			// keep initial total issuance to assert later.
			let asset_total_issuance = Assets::total_issuance(asset_1);
			let native_total_issuance = Balances::total_issuance();

			// prepare input to buy weight.
			let weight = Weight::from_parts(4_000_000_000, 0);
			let fee = WeightToFee::weight_to_fee(&weight);
			let asset_fee = AssetConversion::get_amount_in(
				<Runtime as pallet_asset_conversion::Config>::LPFee::get(),
				&fee,
				&pool_liquidity,
				&pool_liquidity,
			)
			.unwrap();
			let extra_amount = 100;
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
			let payment: Asset = (asset_1_location.clone(), asset_fee + extra_amount).into();

			// init trader and buy weight.
			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let unused_asset = trader
				.buy_weight(weight, asset_to_holding_withdraw(payment, &bob), &ctx)
				.expect("Expected Ok");

			// assert.
			let unused_amount = unused_asset
				.fungible
				.get(&asset_1_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(unused_amount, extra_amount);
			assert_eq!(Assets::total_issuance(asset_1), asset_total_issuance);

			// prepare input to refund weight.
			let refund_weight = Weight::from_parts(1_000_000_000, 0);
			let refund = WeightToFee::weight_to_fee(&refund_weight);
			let (reserve1, reserve2) = AssetConversion::get_reserves(
				Location::try_from(native_location).expect("conversion works"),
				Location::try_from(asset_1_location.clone()).expect("conversion works"),
			)
			.unwrap();
			let asset_refund = AssetConversion::get_amount_out(
				<Runtime as pallet_asset_conversion::Config>::LPFee::get(),
				&refund,
				&reserve1,
				&reserve2,
			)
			.unwrap();

			// refund.
			let actual_refund = trader.refund_weight(refund_weight, &ctx).unwrap();
			let expected_refund = asset_to_holding((asset_1_location, asset_refund).into());
			assert_eq!(actual_refund, expected_refund);

			// assert.
			assert_eq!(Balances::balance(&staking_pot), initial_balance);
			// only after `trader` is dropped we expect the fee to be resolved into the treasury
			// account.
			drop(trader);
			assert_eq!(Balances::balance(&staking_pot), initial_balance + fee - refund);
			assert_eq!(Assets::total_issuance(asset_1), asset_total_issuance);
			assert_eq!(Balances::total_issuance(), native_total_issuance);
		})
}

#[test]
fn test_buy_and_refund_weight_with_swap_foreign_asset_xcm_trader() {
	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let bob: AccountId = SOME_ASSET_ADMIN.into();
			let staking_pot = CollatorSelection::account_id();
			let native_location =
				Location::try_from(TokenLocation::get()).expect("conversion works");
			let foreign_location = Location {
				parents: 1,
				interior: (Junction::Parachain(1234), Junction::GeneralIndex(12345)).into(),
			};
			// bob's initial balance for native and `asset1` assets.
			let initial_balance = 200 * UNITS;
			// liquidity for both arms of (native, asset1) pool.
			let pool_liquidity = 100 * UNITS;

			// init asset, balances and pool.
			assert_ok!(<ForeignAssets as Create<_>>::create(
				foreign_location.clone(),
				bob.clone(),
				true,
				10
			));

			assert_ok!(ForeignAssets::mint_into(foreign_location.clone(), &bob, initial_balance));
			assert_ok!(Balances::mint_into(&bob, initial_balance));
			assert_ok!(Balances::mint_into(&staking_pot, initial_balance));

			assert_ok!(AssetConversion::create_pool(
				RuntimeHelper::origin_of(bob.clone()),
				Box::new(native_location.clone()),
				Box::new(foreign_location.clone())
			));

			assert_ok!(AssetConversion::add_liquidity(
				RuntimeHelper::origin_of(bob.clone()),
				Box::new(native_location.clone()),
				Box::new(foreign_location.clone()),
				pool_liquidity,
				pool_liquidity,
				1,
				1,
				bob.clone(),
			));

			// keep initial total issuance to assert later.
			let asset_total_issuance = ForeignAssets::total_issuance(foreign_location.clone());
			let native_total_issuance = Balances::total_issuance();

			// prepare input to buy weight.
			let weight = Weight::from_parts(4_000_000_000, 0);
			let fee = WeightToFee::weight_to_fee(&weight);
			let asset_fee = AssetConversion::get_amount_in(
				<Runtime as pallet_asset_conversion::Config>::LPFee::get(),
				&fee,
				&pool_liquidity,
				&pool_liquidity,
			)
			.unwrap();
			let extra_amount = 100;
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
			let payment: Asset = (foreign_location.clone(), asset_fee + extra_amount).into();

			// init trader and buy weight.
			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let unused_asset = trader
				.buy_weight(weight, asset_to_holding_withdraw(payment, &bob), &ctx)
				.expect("Expected Ok");

			// assert.
			let unused_amount = unused_asset
				.fungible
				.get(&foreign_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(unused_amount, extra_amount);
			assert_eq!(
				ForeignAssets::total_issuance(foreign_location.clone()),
				asset_total_issuance
			);

			// prepare input to refund weight.
			let refund_weight = Weight::from_parts(1_000_000_000, 0);
			let refund = WeightToFee::weight_to_fee(&refund_weight);
			let (reserve1, reserve2) =
				AssetConversion::get_reserves(native_location, foreign_location.clone()).unwrap();
			let asset_refund = AssetConversion::get_amount_out(
				<Runtime as pallet_asset_conversion::Config>::LPFee::get(),
				&refund,
				&reserve1,
				&reserve2,
			)
			.unwrap();

			// refund.
			let actual_refund = trader.refund_weight(refund_weight, &ctx).unwrap();
			let expected_refund = asset_to_holding((foreign_location.clone(), asset_refund).into());
			assert_eq!(actual_refund, expected_refund);

			// assert.
			assert_eq!(Balances::balance(&staking_pot), initial_balance);
			// only after `trader` is dropped we expect the fee to be resolved into the treasury
			// account.
			drop(trader);
			assert_eq!(Balances::balance(&staking_pot), initial_balance + fee - refund);
			assert_eq!(ForeignAssets::total_issuance(foreign_location), asset_total_issuance);
			assert_eq!(Balances::total_issuance(), native_total_issuance);
		})
}

#[test]
fn test_asset_xcm_take_first_trader_refund_not_possible_since_amount_less_than_ed() {
	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			// We need root origin to create a sufficient asset
			// We set existential deposit to be identical to the one for Balances first
			assert_ok!(Assets::force_create(
				RuntimeHelper::root_origin(),
				1.into(),
				AccountId::from(ALICE).into(),
				true,
				ExistentialDeposit::get()
			));

			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

			// Set Alice as block author, who will receive fees
			RuntimeHelper::run_to_block(2, AccountId::from(ALICE));

			// We are going to buy small amount
			let bought = Weight::from_parts(500_000_000u64, 0);

			let asset_location = AssetIdForTrustBackedAssetsConvert::convert_back(&1).unwrap();

			let amount_bought = WeightToFee::weight_to_fee(&bought);

			assert!(
				amount_bought < ExistentialDeposit::get(),
				"we are testing what happens when the amount does not exceed ED"
			);

			let asset: Asset = (asset_location.clone(), amount_bought).into();

			// Mint the asset to alice so we can withdraw it
			// Need to mint at least ED to satisfy minimum balance requirement
			let mint_amount = amount_bought.max(ExistentialDeposit::get() + 1);
			assert_ok!(Assets::mint(
				RuntimeHelper::origin_of(AccountId::from(ALICE)),
				1.into(),
				AccountId::from(ALICE).into(),
				mint_amount
			));

			// Withdraw to create proper AssetsInHolding
			let alice_location: Location =
				Junction::AccountId32 { network: None, id: ALICE.into() }.into();
			let asset_holding =
				<XcmConfig as xcm_executor::Config>::AssetTransactor::withdraw_asset(
					&asset,
					&alice_location,
					Some(&ctx),
				)
				.expect("Failed to withdraw asset");

			// Buy weight should return an error (asset is returned in error)
			let result = trader.buy_weight(bought, asset_holding, &ctx);
			assert!(result.is_err());
			if let Err((returned_asset, xcm_error)) = result {
				assert_eq!(xcm_error, XcmError::TooExpensive);
				// The asset should be returned (we minted mint_amount, so expect that back)
				assert_eq!(
					returned_asset.fungible.get(&asset_location.into()).map_or(0, |a| a.amount()),
					mint_amount
				);
			}

			// not credited since the ED is higher than this value
			assert_eq!(Assets::balance(1, AccountId::from(ALICE)), 0);

			// We also need to ensure the total supply did not increase
			assert_eq!(Assets::total_supply(1), 0);
		});
}

#[test]
fn test_asset_xcm_trader_not_possible_for_non_sufficient_assets() {
	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			// Create a non-sufficient asset with specific existential deposit
			let minimum_asset_balance = 1_000_000_u128;
			assert_ok!(Assets::force_create(
				RuntimeHelper::root_origin(),
				1.into(),
				AccountId::from(ALICE).into(),
				false,
				minimum_asset_balance
			));

			// We first mint enough asset for the account to exist for assets
			assert_ok!(Assets::mint(
				RuntimeHelper::origin_of(AccountId::from(ALICE)),
				1.into(),
				AccountId::from(ALICE).into(),
				minimum_asset_balance
			));

			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

			// Set Alice as block author, who will receive fees
			RuntimeHelper::run_to_block(2, AccountId::from(ALICE));

			// We are going to buy 4e9 weight
			let bought = Weight::from_parts(4_000_000_000u64, 0);

			// lets calculate amount needed
			let asset_amount_needed = WeightToFee::weight_to_fee(&bought);

			let asset_location = AssetIdForTrustBackedAssetsConvert::convert_back(&1).unwrap();

			let asset: Asset = (asset_location.clone(), asset_amount_needed).into();

			// Mint additional asset to alice for this test
			assert_ok!(Assets::mint(
				RuntimeHelper::origin_of(AccountId::from(ALICE)),
				1.into(),
				AccountId::from(ALICE).into(),
				asset_amount_needed
			));

			// Withdraw to create proper AssetsInHolding
			let alice_location: Location =
				Junction::AccountId32 { network: None, id: ALICE.into() }.into();
			let asset_holding =
				<XcmConfig as xcm_executor::Config>::AssetTransactor::withdraw_asset(
					&asset,
					&alice_location,
					Some(&ctx),
				)
				.expect("Failed to withdraw asset");

			// Make sure buy_weight returns an error (asset is returned in error)
			let result = trader.buy_weight(bought, asset_holding, &ctx);
			assert!(result.is_err());
			if let Err((returned_asset, xcm_error)) = result {
				assert_eq!(xcm_error, XcmError::TooExpensive);
				// The asset should be returned
				assert_eq!(
					returned_asset.fungible.get(&asset_location.into()).map_or(0, |a| a.amount()),
					asset_amount_needed
				);
			}

			// Drop trader
			drop(trader);

			// Make sure author(Alice) has NOT received the amount
			assert_eq!(Assets::balance(1, AccountId::from(ALICE)), minimum_asset_balance);

			// We also need to ensure the total supply NOT increased
			assert_eq!(Assets::total_supply(1), minimum_asset_balance);
		});
}

#[test]
fn test_assets_balances_api_works() {
	use assets_common::runtime_api::runtime_decl_for_fungibles_api::FungiblesApi;

	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let local_asset_id = 1;
			let foreign_asset_id_location =
				Location::new(1, [Junction::Parachain(1234), Junction::GeneralIndex(12345)]);

			// check before
			assert_eq!(Assets::balance(local_asset_id, AccountId::from(ALICE)), 0);
			assert_eq!(
				ForeignAssets::balance(foreign_asset_id_location.clone(), AccountId::from(ALICE)),
				0
			);
			assert_eq!(Balances::free_balance(AccountId::from(ALICE)), 0);
			assert!(Runtime::query_account_balances(AccountId::from(ALICE))
				.unwrap()
				.try_as::<XcmAssets>()
				.unwrap()
				.is_none());

			// Drip some balance
			use frame_support::traits::fungible::Mutate;
			let some_currency = ExistentialDeposit::get();
			Balances::mint_into(&AccountId::from(ALICE), some_currency).unwrap();

			// We need root origin to create a sufficient asset
			let minimum_asset_balance = 3333333_u128;
			assert_ok!(Assets::force_create(
				RuntimeHelper::root_origin(),
				local_asset_id.into(),
				AccountId::from(ALICE).into(),
				true,
				minimum_asset_balance
			));

			// We first mint enough asset for the account to exist for assets
			assert_ok!(Assets::mint(
				RuntimeHelper::origin_of(AccountId::from(ALICE)),
				local_asset_id.into(),
				AccountId::from(ALICE).into(),
				minimum_asset_balance
			));

			// create foreign asset
			let foreign_asset_minimum_asset_balance = 3333333_u128;
			assert_ok!(ForeignAssets::force_create(
				RuntimeHelper::root_origin(),
				foreign_asset_id_location.clone(),
				AccountId::from(SOME_ASSET_ADMIN).into(),
				false,
				foreign_asset_minimum_asset_balance
			));

			// We first mint enough asset for the account to exist for assets
			assert_ok!(ForeignAssets::mint(
				RuntimeHelper::origin_of(AccountId::from(SOME_ASSET_ADMIN)),
				foreign_asset_id_location.clone(),
				AccountId::from(ALICE).into(),
				6 * foreign_asset_minimum_asset_balance
			));

			// check after
			assert_eq!(
				Assets::balance(local_asset_id, AccountId::from(ALICE)),
				minimum_asset_balance
			);
			assert_eq!(
				ForeignAssets::balance(foreign_asset_id_location.clone(), AccountId::from(ALICE)),
				6 * minimum_asset_balance
			);
			assert_eq!(Balances::free_balance(AccountId::from(ALICE)), some_currency);

			let result: XcmAssets = Runtime::query_account_balances(AccountId::from(ALICE))
				.unwrap()
				.try_into()
				.unwrap();
			assert_eq!(result.len(), 3);

			// check currency
			assert!(result.inner().iter().any(|asset| asset.eq(
				&assets_common::fungible_conversion::convert_balance::<TokenLocation, Balance>(
					some_currency
				)
				.unwrap()
			)));
			// check trusted asset
			assert!(result.inner().iter().any(|asset| asset.eq(&(
				AssetIdForTrustBackedAssetsConvert::convert_back(&local_asset_id).unwrap(),
				minimum_asset_balance
			)
				.into())));
			// check foreign asset
			assert!(result.inner().iter().any(|asset| asset.eq(&(
				WithLatestLocationConverter::<Location>::convert_back(&foreign_asset_id_location)
					.unwrap(),
				6 * foreign_asset_minimum_asset_balance
			)
				.into())));
		});
}

asset_test_utils::include_teleports_for_native_asset_works!(
	Runtime,
	AllPalletsWithoutSystem,
	XcmConfig,
	(),
	WeightToFee,
	ParachainSystem,
	collator_session_keys(),
	slot_durations(),
	ExistentialDeposit::get(),
	Box::new(|runtime_event_encoded: Vec<u8>| {
		match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
			Ok(RuntimeEvent::PolkadotXcm(event)) => Some(event),
			_ => None,
		}
	}),
	1000
);

asset_test_utils::include_teleports_for_foreign_assets_works!(
	Runtime,
	AllPalletsWithoutSystem,
	XcmConfig,
	CheckingAccount,
	WeightToFee,
	ParachainSystem,
	LocationToAccountId,
	ForeignAssetsInstance,
	collator_session_keys(),
	slot_durations(),
	ExistentialDeposit::get(),
	Box::new(|runtime_event_encoded: Vec<u8>| {
		match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
			Ok(RuntimeEvent::PolkadotXcm(event)) => Some(event),
			_ => None,
		}
	}),
	Box::new(|runtime_event_encoded: Vec<u8>| {
		match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
			Ok(RuntimeEvent::XcmpQueue(event)) => Some(event),
			_ => None,
		}
	})
);

asset_test_utils::include_asset_transactor_transfer_with_local_consensus_currency_works!(
	Runtime,
	XcmConfig,
	collator_session_keys(),
	ExistentialDeposit::get(),
	Box::new(|| {
		assert!(Assets::asset_ids().collect::<Vec<_>>().is_empty());
		assert!(ForeignAssets::asset_ids().collect::<Vec<_>>().is_empty());
	}),
	Box::new(|| {
		assert!(Assets::asset_ids().collect::<Vec<_>>().is_empty());
		assert!(ForeignAssets::asset_ids().collect::<Vec<_>>().is_empty());
	})
);

asset_test_utils::include_asset_transactor_transfer_with_pallet_assets_instance_works!(
	asset_transactor_transfer_with_trust_backed_assets_works,
	Runtime,
	XcmConfig,
	TrustBackedAssetsInstance,
	AssetIdForTrustBackedAssets,
	AssetIdForTrustBackedAssetsConvert,
	collator_session_keys(),
	ExistentialDeposit::get(),
	12345,
	Box::new(|| {
		assert!(ForeignAssets::asset_ids().collect::<Vec<_>>().is_empty());
	}),
	Box::new(|| {
		assert!(ForeignAssets::asset_ids().collect::<Vec<_>>().is_empty());
	})
);

asset_test_utils::include_asset_transactor_transfer_with_pallet_assets_instance_works!(
	asset_transactor_transfer_with_foreign_assets_works,
	Runtime,
	XcmConfig,
	ForeignAssetsInstance,
	Location,
	TryConvertInto,
	collator_session_keys(),
	ExistentialDeposit::get(),
	Location::new(1, [Junction::Parachain(1313), Junction::GeneralIndex(12345)]),
	Box::new(|| {
		assert!(Assets::asset_ids().collect::<Vec<_>>().is_empty());
	}),
	Box::new(|| {
		assert!(Assets::asset_ids().collect::<Vec<_>>().is_empty());
	})
);

asset_test_utils::include_create_and_manage_foreign_assets_for_local_consensus_parachain_assets_works!(
	Runtime,
	XcmConfig,
	WeightToFee,
	LocationToAccountId,
	ForeignAssetsInstance,
	Location,
	WithLatestLocationConverter<Location>,
	collator_session_keys(),
	ExistentialDeposit::get(),
	AssetDeposit::get(),
	MetadataDepositBase::get(),
	MetadataDepositPerByte::get(),
	Box::new(|pallet_asset_call| RuntimeCall::ForeignAssets(pallet_asset_call).encode()),
	Box::new(|runtime_event_encoded: Vec<u8>| {
		match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
			Ok(RuntimeEvent::ForeignAssets(pallet_asset_event)) => Some(pallet_asset_event),
			_ => None,
		}
	}),
	Box::new(|| {
		assert!(Assets::asset_ids().collect::<Vec<_>>().is_empty());
		assert!(ForeignAssets::asset_ids().collect::<Vec<_>>().is_empty());
	}),
	Box::new(|| {
		assert!(Assets::asset_ids().collect::<Vec<_>>().is_empty());
		assert_eq!(ForeignAssets::asset_ids().collect::<Vec<_>>().len(), 1);
	})
);

fn limited_reserve_transfer_assets_for_native_asset_over_bridge_works(
	bridging_configuration: fn() -> TestBridgingConfig,
) {
	asset_test_utils::test_cases_over_bridge::limited_reserve_transfer_assets_for_native_asset_works::<
		Runtime,
		AllPalletsWithoutSystem,
		XcmConfig,
		ParachainSystem,
		XcmpQueue,
		LocationToAccountId,
	>(
		collator_session_keys(),
		slot_durations(),
		ExistentialDeposit::get(),
		AccountId::from(ALICE),
		Box::new(|runtime_event_encoded: Vec<u8>| {
			match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
				Ok(RuntimeEvent::PolkadotXcm(event)) => Some(event),
				_ => None,
			}
		}),
		Box::new(|runtime_event_encoded: Vec<u8>| {
			match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
				Ok(RuntimeEvent::XcmpQueue(event)) => Some(event),
				_ => None,
			}
		}),
		bridging_configuration,
		WeightLimit::Unlimited,
		None,
		Some(xcm_config::TreasuryAccount::get()),
	)
}

mod asset_hub_rococo_tests {
	use super::*;
	use asset_hub_rococo_runtime::{ForeignAssetReserveData, PolkadotXcm};
	use xcm::latest::WESTEND_GENESIS_HASH;
	use xcm_executor::traits::ConvertLocation;

	fn bridging_to_asset_hub_westend() -> TestBridgingConfig {
		let _ = PolkadotXcm::force_xcm_version(
			RuntimeOrigin::root(),
			Box::new(bridging::to_westend::AssetHubWestend::get()),
			XCM_VERSION,
		)
		.expect("version saved!");
		TestBridgingConfig {
			bridged_network: bridging::to_westend::WestendNetwork::get(),
			local_bridge_hub_para_id: bridging::SiblingBridgeHubParaId::get(),
			local_bridge_hub_location: bridging::SiblingBridgeHub::get(),
			bridged_target_location: bridging::to_westend::AssetHubWestend::get(),
		}
	}

	#[test]
	fn limited_reserve_transfer_assets_for_native_asset_to_asset_hub_westend_works() {
		limited_reserve_transfer_assets_for_native_asset_over_bridge_works(
			bridging_to_asset_hub_westend,
		)
	}

	#[test]
	fn receive_reserve_asset_deposited_wnd_from_asset_hub_westend_fees_paid_by_pool_swap_works() {
		const BLOCK_AUTHOR_ACCOUNT: [u8; 32] = [13; 32];
		let block_author_account = AccountId::from(BLOCK_AUTHOR_ACCOUNT);
		let staking_pot = StakingPot::get();

		let foreign_asset_id_location =
			Location::new(2, [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH))]);
		let reserve_location =
			Location::new(2, [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)), Parachain(1000)]);
		let foreign_asset_reserve_data =
			ForeignAssetReserveData { reserve: reserve_location, teleportable: false };
		let foreign_asset_id_minimum_balance = 1_000_000_000;
		// sovereign account as foreign asset owner (can be whoever for this scenario)
		let foreign_asset_owner =
			LocationToAccountId::convert_location(&Location::parent()).unwrap();
		let foreign_asset_create_params = (
			foreign_asset_owner.clone(),
			foreign_asset_id_location.clone(),
			foreign_asset_reserve_data,
			foreign_asset_id_minimum_balance,
		);
		let pool_params = (
			foreign_asset_owner,
			foreign_asset_id_location.clone(),
			foreign_asset_id_minimum_balance,
		);

		asset_test_utils::test_cases_over_bridge::receive_reserve_asset_deposited_from_different_consensus_works::<
			Runtime,
			AllPalletsWithoutSystem,
			XcmConfig,
			ForeignAssetsInstance,
		>(
			collator_session_keys().add(collator_session_key(BLOCK_AUTHOR_ACCOUNT)),
			ExistentialDeposit::get(),
			AccountId::from([73; 32]),
			block_author_account,
			// receiving WNDs
			foreign_asset_create_params,
			1000000000000,
			|| {
				// setup pool for paying fees to touch `SwapFirstAssetTrader`
				asset_test_utils::test_cases::setup_pool_for_paying_fees_with_foreign_assets::<Runtime, RuntimeOrigin>(ExistentialDeposit::get(), pool_params);
				// staking pot account for collecting local native fees from `BuyExecution`
				let _ = Balances::force_set_balance(RuntimeOrigin::root(), StakingPot::get().into(), ExistentialDeposit::get());
				// prepare bridge configuration
				bridging_to_asset_hub_westend()
			},
			(
				[PalletInstance(bp_bridge_hub_rococo::WITH_BRIDGE_ROCOCO_TO_WESTEND_MESSAGES_PALLET_INDEX)].into(),
				GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
				[Parachain(1000)].into()
			),
			|| {
				// check staking pot for ED
				assert_eq!(Balances::free_balance(&staking_pot), ExistentialDeposit::get());
				// check now foreign asset for staking pot
				assert_eq!(
					ForeignAssets::balance(
						foreign_asset_id_location.clone().into(),
						&staking_pot
					),
					0
				);
			},
			|| {
				// `SwapFirstAssetTrader` - staking pot receives xcm fees in ROCs
				assert!(
					Balances::free_balance(&staking_pot) > ExistentialDeposit::get()
				);
				// staking pot receives no foreign assets
				assert_eq!(
					ForeignAssets::balance(
						foreign_asset_id_location.clone().into(),
						&staking_pot
					),
					0
				);
			}
		)
	}

	#[test]
	fn receive_reserve_asset_deposited_wnd_from_asset_hub_westend_fees_paid_by_sufficient_asset_works(
	) {
		const BLOCK_AUTHOR_ACCOUNT: [u8; 32] = [13; 32];
		let block_author_account = AccountId::from(BLOCK_AUTHOR_ACCOUNT);
		let staking_pot = StakingPot::get();

		let foreign_asset_id_location =
			Location::new(2, [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH))]);
		let reserve_location =
			Location::new(2, [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)), Parachain(1000)]);
		let foreign_asset_reserve_data =
			ForeignAssetReserveData { reserve: reserve_location, teleportable: false };
		let foreign_asset_id_minimum_balance = 1_000_000_000;
		// sovereign account as foreign asset owner (can be whoever for this scenario)
		let foreign_asset_owner =
			LocationToAccountId::convert_location(&Location::parent()).unwrap();
		let foreign_asset_create_params = (
			foreign_asset_owner.clone(),
			foreign_asset_id_location.clone(),
			foreign_asset_reserve_data,
			foreign_asset_id_minimum_balance,
		);
		let pool_params = (
			foreign_asset_owner,
			foreign_asset_id_location.clone(),
			foreign_asset_id_minimum_balance,
		);

		asset_test_utils::test_cases_over_bridge::receive_reserve_asset_deposited_from_different_consensus_works::<
			Runtime,
			AllPalletsWithoutSystem,
			XcmConfig,
			ForeignAssetsInstance,
		>(
			collator_session_keys().add(collator_session_key(BLOCK_AUTHOR_ACCOUNT)),
			ExistentialDeposit::get(),
			AccountId::from([73; 32]),
			block_author_account.clone(),
			// receiving WNDs
			foreign_asset_create_params,
			1000000000000,
			|| {
				asset_test_utils::test_cases::setup_pool_for_paying_fees_with_foreign_assets::<Runtime, RuntimeOrigin>(ExistentialDeposit::get(), pool_params);
				bridging_to_asset_hub_westend()
			},
			(
				[PalletInstance(bp_bridge_hub_rococo::WITH_BRIDGE_ROCOCO_TO_WESTEND_MESSAGES_PALLET_INDEX)].into(),
				GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
				[Parachain(1000)].into()
			),
			|| {
				// check block author before
				assert_eq!(
					ForeignAssets::balance(
						foreign_asset_id_location.clone().into(),
						&block_author_account
					),
					0
				);
			},
			|| {
				// check staking pot has at least ED
				assert!(Balances::free_balance(&staking_pot) >= ExistentialDeposit::get());
				// check now foreign asset for staking pot
				assert_eq!(
					ForeignAssets::balance(
						foreign_asset_id_location.clone().into(),
						&staking_pot
					),
					0
				);
			}
		)
	}

	#[test]
	fn report_bridge_status_from_xcm_bridge_router_for_westend_works() {
		asset_test_utils::test_cases_over_bridge::report_bridge_status_from_xcm_bridge_router_works::<
			Runtime,
			AllPalletsWithoutSystem,
			XcmConfig,
			LocationToAccountId,
			ToWestendXcmRouterInstance,
		>(
			collator_session_keys(),
			bridging_to_asset_hub_westend,
			|| bp_asset_hub_rococo::build_congestion_message(Default::default(), true).into(),
			|| bp_asset_hub_rococo::build_congestion_message(Default::default(), false).into(),
		)
	}

	#[test]
	fn test_report_bridge_status_call_compatibility() {
		// if this test fails, make sure `bp_asset_hub_rococo` has valid encoding
		assert_eq!(
			RuntimeCall::ToWestendXcmRouter(
				pallet_xcm_bridge_hub_router::Call::report_bridge_status {
					bridge_id: Default::default(),
					is_congested: true,
				}
			)
			.encode(),
			bp_asset_hub_rococo::Call::ToWestendXcmRouter(
				bp_asset_hub_rococo::XcmBridgeHubRouterCall::report_bridge_status {
					bridge_id: Default::default(),
					is_congested: true,
				}
			)
			.encode()
		);
	}

	#[test]
	fn check_sane_weight_report_bridge_status_for_westend() {
		use pallet_xcm_bridge_hub_router::WeightInfo;
		let actual = <Runtime as pallet_xcm_bridge_hub_router::Config<
			ToWestendXcmRouterInstance,
		>>::WeightInfo::report_bridge_status();
		let max_weight = bp_asset_hub_rococo::XcmBridgeHubRouterTransactCallMaxWeight::get();
		assert!(
			actual.all_lte(max_weight),
			"max_weight: {:?} should be adjusted to actual {:?}",
			max_weight,
			actual
		);
	}

	#[test]
	fn reserve_transfer_native_asset_to_non_teleport_para_works() {
		asset_test_utils::test_cases::reserve_transfer_native_asset_to_non_teleport_para_works::<
			Runtime,
			AllPalletsWithoutSystem,
			XcmConfig,
			ParachainSystem,
			XcmpQueue,
			LocationToAccountId,
		>(
			collator_session_keys(),
			slot_durations(),
			ExistentialDeposit::get(),
			AccountId::from(ALICE),
			Box::new(|runtime_event_encoded: Vec<u8>| {
				match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
					Ok(RuntimeEvent::PolkadotXcm(event)) => Some(event),
					_ => None,
				}
			}),
			Box::new(|runtime_event_encoded: Vec<u8>| {
				match RuntimeEvent::decode(&mut &runtime_event_encoded[..]) {
					Ok(RuntimeEvent::XcmpQueue(event)) => Some(event),
					_ => None,
				}
			}),
			WeightLimit::Unlimited,
		);
	}
}

#[test]
fn change_xcm_bridge_hub_router_byte_fee_by_governance_works() {
	asset_test_utils::test_cases::change_storage_constant_by_governance_works::<
		Runtime,
		bridging::XcmBridgeHubRouterByteFee,
		Balance,
	>(
		collator_session_keys(),
		1000,
		Governance::get(),
		|| {
			(
				bridging::XcmBridgeHubRouterByteFee::key().to_vec(),
				bridging::XcmBridgeHubRouterByteFee::get(),
			)
		},
		|old_value| {
			if let Some(new_value) = old_value.checked_add(1) {
				new_value
			} else {
				old_value.checked_sub(1).unwrap()
			}
		},
	)
}

#[test]
fn change_xcm_bridge_hub_router_base_fee_by_governance_works() {
	asset_test_utils::test_cases::change_storage_constant_by_governance_works::<
		Runtime,
		bridging::XcmBridgeHubRouterBaseFee,
		Balance,
	>(
		collator_session_keys(),
		1000,
		Governance::get(),
		|| {
			tracing::error!(
				target: "bridges::estimate",
				actual_value=%bridging::XcmBridgeHubRouterBaseFee::get(),
				runtime=%<Runtime as frame_system::Config>::Version::get(),
				"`bridging::XcmBridgeHubRouterBaseFee`"
			);
			(
				bridging::XcmBridgeHubRouterBaseFee::key().to_vec(),
				bridging::XcmBridgeHubRouterBaseFee::get(),
			)
		},
		|old_value| {
			if let Some(new_value) = old_value.checked_add(1) {
				new_value
			} else {
				old_value.checked_sub(1).unwrap()
			}
		},
	)
}

#[test]
fn change_xcm_bridge_hub_ethereum_base_fee_by_governance_works() {
	asset_test_utils::test_cases::change_storage_constant_by_governance_works::<
		Runtime,
		bridging::to_ethereum::BridgeHubEthereumBaseFee,
		Balance,
	>(
		collator_session_keys(),
		1000,
		Governance::get(),
		|| {
			tracing::error!(
				target: "bridges::estimate",
				actual_value=%bridging::to_ethereum::BridgeHubEthereumBaseFee::get(),
				runtime=%<Runtime as frame_system::Config>::Version::get(),
				"`bridging::BridgeHubEthereumBaseFee`"
			);
			(
				bridging::to_ethereum::BridgeHubEthereumBaseFee::key().to_vec(),
				bridging::to_ethereum::BridgeHubEthereumBaseFee::get(),
			)
		},
		|old_value| {
			if let Some(new_value) = old_value.checked_add(1) {
				new_value
			} else {
				old_value.checked_sub(1).unwrap()
			}
		},
	)
}

#[test]
fn location_conversion_works() {
	// the purpose of hardcoded values is to catch an unintended location conversion logic change.
	struct TestCase {
		description: &'static str,
		location: Location,
		expected_account_id_str: &'static str,
	}

	let test_cases = vec![
		// DescribeTerminus
		TestCase {
			description: "DescribeTerminus Parent",
			location: Location::new(1, Here),
			expected_account_id_str: "5Dt6dpkWPwLaH4BBCKJwjiWrFVAGyYk3tLUabvyn4v7KtESG",
		},
		TestCase {
			description: "DescribeTerminus Sibling",
			location: Location::new(1, [Parachain(1111)]),
			expected_account_id_str: "5Eg2fnssmmJnF3z1iZ1NouAuzciDaaDQH7qURAy3w15jULDk",
		},
		// DescribePalletTerminal
		TestCase {
			description: "DescribePalletTerminal Parent",
			location: Location::new(1, [PalletInstance(50)]),
			expected_account_id_str: "5CnwemvaAXkWFVwibiCvf2EjqwiqBi29S5cLLydZLEaEw6jZ",
		},
		TestCase {
			description: "DescribePalletTerminal Sibling",
			location: Location::new(1, [Parachain(1111), PalletInstance(50)]),
			expected_account_id_str: "5GFBgPjpEQPdaxEnFirUoa51u5erVx84twYxJVuBRAT2UP2g",
		},
		// DescribeAccountId32Terminal
		TestCase {
			description: "DescribeAccountId32Terminal Parent",
			location: Location::new(
				1,
				[AccountId32 { network: None, id: AccountId::from(ALICE).into() }],
			),
			expected_account_id_str: "5DN5SGsuUG7PAqFL47J9meViwdnk9AdeSWKFkcHC45hEzVz4",
		},
		TestCase {
			description: "DescribeAccountId32Terminal Sibling",
			location: Location::new(
				1,
				[
					Parachain(1111),
					Junction::AccountId32 { network: None, id: AccountId::from(ALICE).into() },
				],
			),
			expected_account_id_str: "5DGRXLYwWGce7wvm14vX1Ms4Vf118FSWQbJkyQigY2pfm6bg",
		},
		// DescribeAccountKey20Terminal
		TestCase {
			description: "DescribeAccountKey20Terminal Parent",
			location: Location::new(1, [AccountKey20 { network: None, key: [0u8; 20] }]),
			expected_account_id_str: "5F5Ec11567pa919wJkX6VHtv2ZXS5W698YCW35EdEbrg14cg",
		},
		TestCase {
			description: "DescribeAccountKey20Terminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), AccountKey20 { network: None, key: [0u8; 20] }],
			),
			expected_account_id_str: "5CB2FbUds2qvcJNhDiTbRZwiS3trAy6ydFGMSVutmYijpPAg",
		},
		// DescribeTreasuryVoiceTerminal
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Parent",
			location: Location::new(1, [Plurality { id: BodyId::Treasury, part: BodyPart::Voice }]),
			expected_account_id_str: "5CUjnE2vgcUCuhxPwFoQ5r7p1DkhujgvMNDHaF2bLqRp4D5F",
		},
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), Plurality { id: BodyId::Treasury, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5G6TDwaVgbWmhqRUKjBhRRnH4ry9L9cjRymUEmiRsLbSE4gB",
		},
		// DescribeBodyTerminal
		TestCase {
			description: "DescribeBodyTerminal Parent",
			location: Location::new(1, [Plurality { id: BodyId::Unit, part: BodyPart::Voice }]),
			expected_account_id_str: "5EBRMTBkDisEXsaN283SRbzx9Xf2PXwUxxFCJohSGo4jYe6B",
		},
		TestCase {
			description: "DescribeBodyTerminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), Plurality { id: BodyId::Unit, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5DBoExvojy8tYnHgLL97phNH975CyT45PWTZEeGoBZfAyRMH",
		},
		// ExternalConsensusLocationsConverterFor
		TestCase {
			description: "Describe Ethereum Location",
			location: Location::new(2, [GlobalConsensus(Ethereum { chain_id: 11155111 })]),
			expected_account_id_str: "5GjRnmh5o3usSYzVmsxBWzHEpvJyHK4tKNPhjpUR3ASrruBy",
		},
		TestCase {
			description: "Describe Ethereum AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(Ethereum { chain_id: 11155111 }),
					AccountKey20 {
						network: None,
						key: hex!("87d1f7fdfEe7f651FaBc8bFCB6E086C278b77A7d"),
					},
				],
			),
			expected_account_id_str: "5HV4j4AsqT349oLRZmTjhGKDofPBWmWaPUfWGaRkuvzkjW9i",
		},
		TestCase {
			description: "Describe Westend Location",
			location: Location::new(2, [GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH))]),
			expected_account_id_str: "5Fb4pyqFuYLZ43USEAcVUBhFTfTckG9zv9kUaVnmR79YgBCe",
		},
		TestCase {
			description: "Describe Westend AccountID",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					AccountId32 { network: None, id: AccountId::from(ALICE).into() },
				],
			),
			expected_account_id_str: "5CpcvNFY6jkMJrd7XQt3yTweRD1WxUeHXvHnbWuVM1MHKHPe",
		},
		TestCase {
			description: "Describe Westend AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					AccountKey20 { network: None, key: [0u8; 20] },
				],
			),
			expected_account_id_str: "5FzaTcFwUMyX5Sfe7wRGuc3zw1cbpGAGZpmAsxS4tBX6x6U3",
		},
		TestCase {
			description: "Describe Westend Treasury Plurality",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					Plurality { id: BodyId::Treasury, part: BodyPart::Voice },
				],
			),
			expected_account_id_str: "5CpdRCmCYwnxS1mifwEddYHDJR8ydDfTpi1gwAQKQvfAjjzu",
		},
		TestCase {
			description: "Describe Westend Parachain Location",
			location: Location::new(
				2,
				[GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)), Parachain(1000)],
			),
			expected_account_id_str: "5CkWf1L181BiSbvoofnzfSg8ZLiBK3i1U4sknzETHk8QS2mA",
		},
		TestCase {
			description: "Describe Westend Parachain AccountID",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					Parachain(1000),
					AccountId32 { network: None, id: AccountId::from(ALICE).into() },
				],
			),
			expected_account_id_str: "5G6JJUm6tgsxJhRn76VGme8WGukdUNiBBK6ABUtH9YXEjEk9",
		},
		TestCase {
			description: "Describe Westend Parachain AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					Parachain(1000),
					AccountKey20 { network: None, key: [0u8; 20] },
				],
			),
			expected_account_id_str: "5EFpSvq8BUAjdjY4tuGhGXZ66P16iQnX7nxsNoHy7TM6NhMa",
		},
		TestCase {
			description: "Describe Westend Parachain Treasury Plurality",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					Parachain(1000),
					Plurality { id: BodyId::Treasury, part: BodyPart::Voice },
				],
			),
			expected_account_id_str: "5GfwA4qaz9wpQPPHmf5MSKqvsPyrfx1yYeeZB1SUkqDuRuZ1",
		},
		TestCase {
			description: "Describe Westend USDT Location",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(WESTEND_GENESIS_HASH)),
					Parachain(1000),
					PalletInstance(50),
					GeneralIndex(1984),
				],
			),
			expected_account_id_str: "5Hd77ZjbVRrYiRXER8qo9DRDB8ZzaKtRswZoypMnMLdixzMs",
		},
	];

	ExtBuilder::<Runtime>::default()
		.with_collators(collator_session_keys().collators())
		.with_session_keys(collator_session_keys().session_keys())
		.with_para_id(1000.into())
		.build()
		.execute_with(|| {
			for tc in test_cases {
				let expected = AccountId::from_string(tc.expected_account_id_str)
					.expect("Invalid AccountId string");

				let got =
					LocationToAccountHelper::<AccountId, LocationToAccountId>::convert_location(
						tc.location.into(),
					)
					.unwrap();

				assert_eq!(got, expected, "{}", tc.description);
			}
		});
}

#[test]
fn xcm_payment_api_works() {
	parachains_runtimes_test_utils::test_cases::xcm_payment_api_with_native_token_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		WeightToFee,
	>();
	asset_test_utils::test_cases::xcm_payment_api_with_pools_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		WeightToFee,
	>();

	asset_test_utils::test_cases::xcm_payment_api_foreign_asset_pool_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		LocationToAccountId,
		Block,
		WeightToFee,
	>(ExistentialDeposit::get(), WESTEND_GENESIS_HASH);
}
