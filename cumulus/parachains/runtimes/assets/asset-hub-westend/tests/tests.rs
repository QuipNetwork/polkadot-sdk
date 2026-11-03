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

//! Tests for the Westmint (Westend Assets Hub) chain.

use alloy_core::{
	primitives::U256,
	sol_types::{sol_data, SolType},
};
use approx::assert_relative_eq;
use asset_hub_westend_runtime::{
	staking, xcm_config,
	xcm_config::{
		bridging, CheckingAccount, LocationToAccountId, StakingPot,
		TrustBackedAssetsPalletLocation, UniquesConvertedConcreteId, UniquesPalletLocation,
		WestendLocation, XcmConfig,
	},
	AllPalletsWithoutSystem, AssetRewards, Assets, Balances, Block, Executive, ExistentialDeposit,
	ForeignAssets, ForeignAssetsInstance, MetadataDepositBase, MetadataDepositPerByte,
	ParachainSystem, PolkadotXcm, Proxy, Revive, Runtime, RuntimeCall, RuntimeEvent, RuntimeOrigin,
	SessionKeys, ToRococoXcmRouterInstance, TrustBackedAssetsInstance, TxExtension,
	UncheckedExtrinsic, Uniques, WeightToFee, XcmpQueue,
};
pub use asset_hub_westend_runtime::{AssetConversion, AssetDeposit, CollatorSelection, System};
use asset_test_utils::{
	test_cases::exchange_asset_on_asset_hub_works, test_cases_over_bridge::TestBridgingConfig,
	CollatorSessionKey, CollatorSessionKeys, ExtBuilder, GovernanceOrigin, SlotDurations,
};
use assets_common::local_and_foreign_assets::ForeignAssetReserveData;
use codec::{Decode, Encode};
use frame_support::{
	assert_err, assert_noop, assert_ok, parameter_types,
	traits::{
		fungible::{self, Inspect, Mutate},
		fungibles::{
			self, Create, Inspect as FungiblesInspect, InspectEnumerable, Mutate as FungiblesMutate,
		},
		tokens::asset_ops::{
			common_strategies::{Bytes, Owner},
			Inspect as InspectUniqueAsset,
		},
		ContainsPair, Hooks, SignedTransactionBuilder,
	},
	weights::{Weight, WeightToFee as WeightToFeeT},
};
use hex_literal::hex;
use pallet_revive::{
	test_utils::builder::{BareInstantiateBuilder, Contract},
	Code, TransactionLimits,
};
use pallet_revive_fixtures::{compile_module, compile_module_with_type, FixtureType};
use pallet_uniques::{asset_ops::Item, asset_strategies::Attribute};
use parachains_common::{AccountId, AssetIdForTrustBackedAssets, AuraId, Balance};
use sp_consensus_aura::SlotDuration;
use sp_core::crypto::Ss58Codec;
use sp_keyring::Sr25519Keyring;
use sp_runtime::{
	generic::Era,
	traits::{MaybeEquivalence, TryConvertInto},
	Either, MultiAddress, MultiSignature,
};
use sp_staking::budget::IssuanceCurve;
use sp_tracing::capture_test_logs;
use std::convert::Into;
use testnet_parachains_constants::westend::{
	consensus::*,
	currency::{CENTS, UNITS},
};
use westend_runtime_constants::system_parachain::ASSET_HUB_ID;
use xcm::{
	latest::{
		prelude::{Assets as XcmAssets, *},
		ROCOCO_GENESIS_HASH,
	},
	VersionedXcm,
};
use xcm_builder::{
	unique_instances::UniqueInstancesAdapter as NewNftAdapter, MatchInClassInstances, NoChecking,
	NonFungiblesAdapter as OldNftAdapter, WithLatestLocationConverter,
};
use xcm_executor::{
	traits::{ConvertLocation, TransactAsset, WeightTrader},
	AssetsInHolding,
};
use xcm_runtime_apis::conversions::LocationToAccountHelper;

use sp_runtime::traits::OpaqueKeys;

const ALICE: [u8; 32] = [1u8; 32];
const BOB: [u8; 32] = [2u8; 32];
const SOME_ASSET_ADMIN: [u8; 32] = [5u8; 32];
const MILLISECONDS_PER_HOUR: u64 = 60 * 60 * 1000;

parameter_types! {
	pub Governance: GovernanceOrigin<RuntimeOrigin> = GovernanceOrigin::Origin(RuntimeOrigin::root());
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

/// Build a bare_instantiate call.
fn bare_instantiate(origin: &AccountId, code: Vec<u8>) -> BareInstantiateBuilder<Runtime> {
	let origin = RuntimeOrigin::signed(origin.clone());
	BareInstantiateBuilder::<Runtime>::bare_instantiate(origin, Code::Upload(code))
}

fn construct_extrinsic(sender: Sr25519Keyring, call: RuntimeCall) -> UncheckedExtrinsic {
	let nonce = frame_system::Pallet::<Runtime>::account(&AccountId::from(sender.public())).nonce;
	construct_extrinsic_with_nonce(sender, call, nonce)
}

fn construct_extrinsic_with_nonce(
	sender: Sr25519Keyring,
	call: RuntimeCall,
	nonce: u32,
) -> UncheckedExtrinsic {
	let account_id = AccountId::from(sender.public());
	let tx_ext: TxExtension = (
		frame_system::AuthorizeCall::<Runtime>::new(),
		frame_system::CheckNonZeroSender::<Runtime>::new(),
		frame_system::CheckSpecVersion::<Runtime>::new(),
		frame_system::CheckTxVersion::<Runtime>::new(),
		frame_system::CheckGenesis::<Runtime>::new(),
		frame_system::CheckEra::<Runtime>::from(Era::immortal()),
		frame_system::CheckNonce::<Runtime>::from(nonce),
		frame_system::CheckWeight::<Runtime>::new(),
		pallet_pgas_allowance::ChargePGAS::<
			Runtime,
			pallet_asset_conversion_tx_payment::ChargeAssetTxPayment<Runtime>,
		>::from(pallet_asset_conversion_tx_payment::ChargeAssetTxPayment::<Runtime>::from(
			0, None,
		)),
		frame_metadata_hash_extension::CheckMetadataHash::new(false),
		Default::default(),
	)
		.into();
	let payload = sp_runtime::generic::SignedPayload::new(call.clone(), tx_ext.clone()).unwrap();
	let signature = payload.using_encoded(|e| sender.sign(e));
	UncheckedExtrinsic::new_signed_transaction(
		call,
		account_id.into(),
		MultiSignature::Sr25519(signature),
		tx_ext,
	)
}

#[test]
fn test_buy_and_refund_weight_in_native() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
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
			let native_location = WestendLocation::get();
			let initial_balance = 200 * UNITS;

			assert_ok!(Balances::mint_into(&bob, initial_balance));
			assert_ok!(Balances::mint_into(&staking_pot, initial_balance));

			// prepare input to buy weight.
			let weight = Weight::from_parts(4_000_000_000, 0);
			let fee = WeightToFee::weight_to_fee(&weight);
			let extra_amount = 100;
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
			let payment: Asset = (native_location.clone(), fee + extra_amount).into();

			// Withdraw from bob to create proper AssetsInHolding with imbalances
			let bob_location: Location =
				Junction::AccountId32 { network: None, id: bob.into() }.into();
			let payment_holding =
				<XcmConfig as xcm_executor::Config>::AssetTransactor::withdraw_asset(
					&payment,
					&bob_location,
					Some(&ctx),
				)
				.expect("Failed to withdraw payment");

			// init trader and buy weight.
			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let unused_asset =
				trader.buy_weight(weight, payment_holding, &ctx).expect("Expected Ok");

			// assert.
			let unused_amount = unused_asset
				.fungible
				.get(&native_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(unused_amount, extra_amount);

			// Record total_issuance after withdraw for accurate final comparison
			let total_issuance_after_withdraw = Balances::total_issuance();

			// prepare input to refund weight.
			let refund_weight = Weight::from_parts(1_000_000_000, 0);
			let refund = WeightToFee::weight_to_fee(&refund_weight);

			// refund.
			let actual_refund = trader.refund_weight(refund_weight, &ctx).unwrap();
			let actual_refund_amount = actual_refund
				.fungible
				.get(&native_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(actual_refund_amount, refund);

			// assert.
			assert_eq!(Balances::balance(&staking_pot), initial_balance);
			// only after `trader` is dropped we expect the fee to be resolved into the treasury
			// account.
			drop(trader);
			assert_eq!(Balances::balance(&staking_pot), initial_balance + fee - refund);
			// With imbalance accounting, total_issuance should match what it was after withdraw
			assert_eq!(Balances::total_issuance(), total_issuance_after_withdraw);
		})
}

#[test]
fn test_buy_and_refund_weight_with_swap_local_asset_xcm_trader() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
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
			let native_location = WestendLocation::get();
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
				Box::new(
					xcm::v5::Location::try_from(native_location.clone()).expect("conversion works")
				),
				Box::new(
					xcm::v5::Location::try_from(asset_1_location.clone())
						.expect("conversion works")
				)
			));

			assert_ok!(AssetConversion::add_liquidity(
				RuntimeHelper::origin_of(bob.clone()),
				Box::new(
					xcm::v5::Location::try_from(native_location.clone()).expect("conversion works")
				),
				Box::new(
					xcm::v5::Location::try_from(asset_1_location.clone())
						.expect("conversion works")
				),
				pool_liquidity,
				pool_liquidity,
				1,
				1,
				bob.clone(),
			));

			// keep initial total issuance to assert later.
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

			// Withdraw from bob to create proper AssetsInHolding with imbalances
			let bob_location: Location =
				Junction::AccountId32 { network: None, id: bob.into() }.into();
			let payment_holding =
				<XcmConfig as xcm_executor::Config>::AssetTransactor::withdraw_asset(
					&payment,
					&bob_location,
					Some(&ctx),
				)
				.expect("Failed to withdraw payment");

			// init trader and buy weight.
			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let unused_asset =
				trader.buy_weight(weight, payment_holding, &ctx).expect("Expected Ok");

			// assert.
			let unused_amount = unused_asset
				.fungible
				.get(&asset_1_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(unused_amount, extra_amount);

			// Record total issuance after withdraw for accurate final comparison
			let asset_total_issuance_after_withdraw = Assets::total_issuance(asset_1);

			// prepare input to refund weight.
			let refund_weight = Weight::from_parts(1_000_000_000, 0);
			let refund = WeightToFee::weight_to_fee(&refund_weight);
			let (reserve1, reserve2) = AssetConversion::get_reserves(
				xcm::v5::Location::try_from(native_location).expect("conversion works"),
				xcm::v5::Location::try_from(asset_1_location.clone()).expect("conversion works"),
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
			let actual_refund_amount = actual_refund
				.fungible
				.get(&asset_1_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(actual_refund_amount, asset_refund);

			// assert.
			assert_eq!(Balances::balance(&staking_pot), initial_balance);
			// only after `trader` is dropped we expect the fee to be resolved into the treasury
			// account.
			drop(trader);
			assert_eq!(Balances::balance(&staking_pot), initial_balance + fee - refund);
			// With imbalance accounting, total_issuance should match what it was after withdraw
			assert_eq!(Assets::total_issuance(asset_1), asset_total_issuance_after_withdraw);
			assert_eq!(Balances::total_issuance(), native_total_issuance);
		})
}

#[test]
fn test_buy_and_refund_weight_with_swap_foreign_asset_xcm_trader() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
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
				xcm::v5::Location::try_from(WestendLocation::get()).expect("conversion works");
			let foreign_location = xcm::v5::Location {
				parents: 1,
				interior: (
					xcm::v5::Junction::Parachain(1234),
					xcm::v5::Junction::GeneralIndex(12345),
				)
					.into(),
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

			// Withdraw from bob to create proper AssetsInHolding with imbalances
			let bob_location: Location =
				Junction::AccountId32 { network: None, id: bob.into() }.into();
			let payment_holding =
				<XcmConfig as xcm_executor::Config>::AssetTransactor::withdraw_asset(
					&payment,
					&bob_location,
					Some(&ctx),
				)
				.expect("Failed to withdraw payment");

			// init trader and buy weight.
			let mut trader = <XcmConfig as xcm_executor::Config>::Trader::new();
			let unused_asset =
				trader.buy_weight(weight, payment_holding, &ctx).expect("Expected Ok");

			// assert.
			let unused_amount = unused_asset
				.fungible
				.get(&foreign_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(unused_amount, extra_amount);

			// Record total issuance after withdraw for accurate final comparison
			let asset_total_issuance_after_withdraw =
				ForeignAssets::total_issuance(foreign_location.clone());

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
			let actual_refund_amount = actual_refund
				.fungible
				.get(&foreign_location.clone().into())
				.map_or(0, |a| a.amount());
			assert_eq!(actual_refund_amount, asset_refund);

			// assert.
			assert_eq!(Balances::balance(&staking_pot), initial_balance);
			// only after `trader` is dropped we expect the fee to be resolved into the treasury
			// account.
			drop(trader);
			assert_eq!(Balances::balance(&staking_pot), initial_balance + fee - refund);
			// With imbalance accounting, total_issuance should match what it was after withdraw
			assert_eq!(
				ForeignAssets::total_issuance(foreign_location),
				asset_total_issuance_after_withdraw
			);
			assert_eq!(Balances::total_issuance(), native_total_issuance);
		})
}

#[test]
fn test_asset_xcm_take_first_trader_refund_not_possible_since_amount_less_than_ed() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
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
fn test_asset_xcm_take_first_trader_not_possible_for_non_sufficient_assets() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
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

fn test_nft_asset_transactor_works<T: TransactAsset>() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let collection_id = 42;
			let item_id = 101;

			let alice = AccountId::from(ALICE);
			let bob = AccountId::from(BOB);
			let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

			assert_ok!(Balances::mint_into(&alice, 2 * UNITS));

			assert_ok!(Uniques::create(
				RuntimeHelper::origin_of(alice.clone()),
				collection_id,
				MultiAddress::Id(alice.clone()),
			));

			assert_ok!(Uniques::mint(
				RuntimeHelper::origin_of(alice.clone()),
				collection_id,
				item_id,
				MultiAddress::Id(bob.clone()),
			));

			let attr_key = vec![0xA, 0xA, 0xB, 0xB];
			let attr_value = vec![0xC, 0x0, 0x0, 0x1, 0xF, 0x0, 0x0, 0xD];

			assert_ok!(Uniques::set_attribute(
				RuntimeHelper::origin_of(alice.clone()),
				collection_id,
				Some(item_id),
				attr_key.clone().try_into().unwrap(),
				attr_value.clone().try_into().unwrap(),
			));

			let collection_location = UniquesPalletLocation::get()
				.appended_with(GeneralIndex(collection_id.into()))
				.unwrap();
			let item_asset: Asset =
				(collection_location.clone(), AssetInstance::Index(item_id.into())).into();

			let alice_account_location: Location = alice.clone().into();
			let bob_account_location: Location = bob.clone().into();

			// Can't deposit the token that isn't withdrawn - create AssetsInHolding for NFT
			let item_holding = AssetsInHolding::new_from_non_fungible(
				collection_location.clone().into(),
				AssetInstance::Index(item_id.into()),
			);
			let deposit_result =
				T::deposit_asset(item_holding, &alice_account_location, Some(&ctx));
			assert!(matches!(deposit_result, Err((_, XcmError::FailedToTransactAsset(_)))));

			// Alice isn't the owner, she can't withdraw the token
			assert_noop!(
				T::withdraw_asset(&item_asset, &alice_account_location, Some(&ctx),),
				XcmError::FailedToTransactAsset("NoPermission")
			);

			// Bob, the owner, can withdraw the token
			let withdrawn_holding =
				T::withdraw_asset(&item_asset, &bob_account_location, Some(&ctx))
					.expect("Withdraw should succeed");

			// The token is withdrawn
			assert_eq!(
				Item::<Uniques>::inspect(&(collection_id, item_id), Owner::default()),
				Err(pallet_uniques::Error::<Runtime>::UnknownItem.into()),
			);

			// But the attribute data is preserved as the pallet-uniques works that way.
			assert_eq!(
				Item::<Uniques>::inspect(
					&(collection_id, item_id),
					Bytes(Attribute(attr_key.as_slice()))
				),
				Ok(attr_value.clone()),
			);

			// Can't withdraw the already withdrawn token
			assert_err!(
				T::withdraw_asset(&item_asset, &bob_account_location, Some(&ctx),),
				XcmError::FailedToTransactAsset("UnknownCollection")
			);

			// Deposit the token to alice using the withdrawn holding
			assert_ok!(T::deposit_asset(withdrawn_holding, &alice_account_location, Some(&ctx),));

			// The token is deposited
			assert_eq!(
				Item::<Uniques>::inspect(&(collection_id, item_id), Owner::default()),
				Ok(alice.clone()),
			);

			// The attribute data is the same
			assert_eq!(
				Item::<Uniques>::inspect(
					&(collection_id, item_id),
					Bytes(Attribute(attr_key.as_slice()))
				),
				Ok(attr_value.clone()),
			);

			// Can't deposit the token twice - create new AssetsInHolding for NFT
			let item_holding_again = AssetsInHolding::new_from_non_fungible(
				collection_location.clone().into(),
				AssetInstance::Index(item_id.into()),
			);
			let deposit_twice_result =
				T::deposit_asset(item_holding_again, &alice_account_location, Some(&ctx));
			assert!(matches!(deposit_twice_result, Err((_, XcmError::FailedToTransactAsset(_)))));

			// Transfer the token directly
			assert_ok!(T::transfer_asset(
				&item_asset,
				&alice_account_location,
				&bob_account_location,
				&ctx,
			));

			// The token's owner has changed
			assert_eq!(
				Item::<Uniques>::inspect(&(collection_id, item_id), Owner::default()),
				Ok(bob.clone()),
			);

			// The attribute data is the same
			assert_eq!(
				Item::<Uniques>::inspect(
					&(collection_id, item_id),
					Bytes(Attribute(attr_key.as_slice()))
				),
				Ok(attr_value.clone()),
			);
		});
}

#[test]
fn test_new_nft_config_works_as_the_old_one() {
	type OldNftTransactor = OldNftAdapter<
		Uniques,
		UniquesConvertedConcreteId,
		LocationToAccountId,
		AccountId,
		NoChecking,
		CheckingAccount,
	>;

	type NewNftTransactor = NewNftAdapter<
		AccountId,
		LocationToAccountId,
		MatchInClassInstances<UniquesConvertedConcreteId>,
		Item<Uniques>,
	>;

	test_nft_asset_transactor_works::<OldNftTransactor>();
	test_nft_asset_transactor_works::<NewNftTransactor>();
}

#[test]
fn test_assets_balances_api_works() {
	use assets_common::runtime_api::runtime_decl_for_fungibles_api::FungiblesApi;

	ExtBuilder::<Runtime>::default()
		.with_tracing()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let local_asset_id = 1;
			let foreign_asset_id_location = xcm::v5::Location {
				parents: 1,
				interior: [
					xcm::v5::Junction::Parachain(1234),
					xcm::v5::Junction::GeneralIndex(12345),
				]
				.into(),
			};

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
			assert!(result.inner().iter().any(|asset| {
				asset.eq(
				&assets_common::fungible_conversion::convert_balance::<WestendLocation, Balance>(
					some_currency
				)
				.unwrap()
			)
			}));
			// check trusted asset
			assert!(result.inner().iter().any(|asset| {
				asset.eq(&(
					AssetIdForTrustBackedAssetsConvert::convert_back(&local_asset_id).unwrap(),
					minimum_asset_balance,
				)
					.into())
			}));
			// check foreign asset
			assert!(result.inner().iter().any(|asset| {
				asset.eq(&(
					WithLatestLocationConverter::<xcm::v5::Location>::convert_back(
						&foreign_asset_id_location,
					)
					.unwrap(),
					6 * foreign_asset_minimum_asset_balance,
				)
					.into())
			}));
		});
}

#[test]
fn authorized_aliases_work() {
	ExtBuilder::<Runtime>::default()
		.with_tracing()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			let alice: AccountId = ALICE.into();
			let local_alice = Location::new(0, AccountId32 { network: None, id: ALICE });
			let alice_on_sibling_para =
				Location::new(1, [Parachain(42), AccountId32 { network: None, id: ALICE }]);
			let alice_on_relay = Location::new(1, AccountId32 { network: None, id: ALICE });
			let bob_on_relay = Location::new(1, AccountId32 { network: None, id: [42_u8; 32] });

			assert_ok!(Balances::mint_into(&alice, 2 * UNITS));

			// neither `alice_on_sibling_para`, `alice_on_relay`, `bob_on_relay` are allowed to
			// alias into `local_alice`
			for aliaser in [&alice_on_sibling_para, &alice_on_relay, &bob_on_relay] {
				assert!(!<XcmConfig as xcm_executor::Config>::Aliasers::contains(
					aliaser,
					&local_alice
				));
			}

			// Alice explicitly authorizes `alice_on_sibling_para` to alias her local account
			assert_ok!(PolkadotXcm::add_authorized_alias(
				RuntimeHelper::origin_of(alice.clone()),
				Box::new(alice_on_sibling_para.clone().into()),
				None
			));

			// `alice_on_sibling_para` now explicitly allowed to alias into `local_alice`
			assert!(<XcmConfig as xcm_executor::Config>::Aliasers::contains(
				&alice_on_sibling_para,
				&local_alice
			));
			// as expected, `alice_on_relay` and `bob_on_relay` still can't alias into `local_alice`
			for aliaser in [&alice_on_relay, &bob_on_relay] {
				assert!(!<XcmConfig as xcm_executor::Config>::Aliasers::contains(
					aliaser,
					&local_alice
				));
			}

			// Alice explicitly authorizes `alice_on_relay` to alias her local account
			assert_ok!(PolkadotXcm::add_authorized_alias(
				RuntimeHelper::origin_of(alice.clone()),
				Box::new(alice_on_relay.clone().into()),
				None
			));
			// Now both `alice_on_relay` and `alice_on_sibling_para` can alias into her local
			// account
			for aliaser in [&alice_on_relay, &alice_on_sibling_para] {
				assert!(<XcmConfig as xcm_executor::Config>::Aliasers::contains(
					aliaser,
					&local_alice
				));
			}

			// Alice removes authorization for `alice_on_relay` to alias her local account
			assert_ok!(PolkadotXcm::remove_authorized_alias(
				RuntimeHelper::origin_of(alice.clone()),
				Box::new(alice_on_relay.clone().into())
			));

			// `alice_on_relay` no longer allowed to alias into `local_alice`
			assert!(!<XcmConfig as xcm_executor::Config>::Aliasers::contains(
				&alice_on_relay,
				&local_alice
			));

			// `alice_on_sibling_para` still allowed to alias into `local_alice`
			assert!(<XcmConfig as xcm_executor::Config>::Aliasers::contains(
				&alice_on_sibling_para,
				&local_alice
			));
		})
}

asset_test_utils::include_teleports_for_native_asset_works!(
	Runtime,
	AllPalletsWithoutSystem,
	XcmConfig,
	CheckingAccount,
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
	xcm::v5::Location,
	TryConvertInto,
	collator_session_keys(),
	ExistentialDeposit::get(),
	xcm::v5::Location {
		parents: 1,
		interior: [xcm::v5::Junction::Parachain(1313), xcm::v5::Junction::GeneralIndex(12345)]
			.into()
	},
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
	xcm::v5::Location,
	WithLatestLocationConverter<xcm::v5::Location>,
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

fn bridging_to_asset_hub_rococo() -> TestBridgingConfig {
	let _ = PolkadotXcm::force_xcm_version(
		RuntimeOrigin::root(),
		Box::new(bridging::to_rococo::AssetHubRococo::get()),
		XCM_VERSION,
	)
	.expect("version saved!");
	TestBridgingConfig {
		bridged_network: bridging::to_rococo::RococoNetwork::get(),
		local_bridge_hub_para_id: bridging::SiblingBridgeHubParaId::get(),
		local_bridge_hub_location: bridging::SiblingBridgeHub::get(),
		bridged_target_location: bridging::to_rococo::AssetHubRococo::get(),
	}
}

#[test]
fn limited_reserve_transfer_assets_for_native_asset_to_asset_hub_rococo_works() {
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
		bridging_to_asset_hub_rococo,
		WeightLimit::Unlimited,
		None,
		Some(xcm_config::DapBufferAccount::get()),
	)
}

#[test]
fn receive_reserve_asset_deposited_roc_from_asset_hub_rococo_fees_paid_by_pool_swap_works() {
	const BLOCK_AUTHOR_ACCOUNT: [u8; 32] = [13; 32];
	let block_author_account = AccountId::from(BLOCK_AUTHOR_ACCOUNT);
	let staking_pot = StakingPot::get();

	let foreign_asset_id_location =
		Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]);
	let reserve_location =
		Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)), Parachain(1000)]);
	let foreign_asset_reserve_data =
		ForeignAssetReserveData { reserve: reserve_location, teleportable: false };
	let foreign_asset_id_minimum_balance = 1_000_000_000;
	// sovereign account as foreign asset owner (can be whoever for this scenario)
	let foreign_asset_owner = LocationToAccountId::convert_location(&Location::parent()).unwrap();
	let foreign_asset_create_params = (
		foreign_asset_owner.clone(),
		foreign_asset_id_location.clone(),
		foreign_asset_reserve_data,
		foreign_asset_id_minimum_balance,
	);
	let pool_params =
		(foreign_asset_owner, foreign_asset_id_location.clone(), foreign_asset_id_minimum_balance);

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
			// receiving ROCs
			foreign_asset_create_params,
			1000000000000,
			|| {
				// setup pool for paying fees to touch `SwapFirstAssetTrader`
				asset_test_utils::test_cases::setup_pool_for_paying_fees_with_foreign_assets::<Runtime, RuntimeOrigin>(ExistentialDeposit::get(), pool_params);
				// staking pot account for collecting local native fees from `BuyExecution`
				let _ = Balances::force_set_balance(RuntimeOrigin::root(), StakingPot::get().into(), ExistentialDeposit::get());
				// prepare bridge configuration
				bridging_to_asset_hub_rococo()
			},
			(
				[PalletInstance(bp_bridge_hub_westend::WITH_BRIDGE_WESTEND_TO_ROCOCO_MESSAGES_PALLET_INDEX)].into(),
				GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
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
fn receive_reserve_asset_deposited_roc_from_asset_hub_rococo_fees_paid_by_sufficient_asset_works() {
	const BLOCK_AUTHOR_ACCOUNT: [u8; 32] = [13; 32];
	let block_author_account = AccountId::from(BLOCK_AUTHOR_ACCOUNT);
	let staking_pot = StakingPot::get();

	let foreign_asset_id_location =
		Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]);
	let reserve_location =
		Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)), Parachain(1000)]);
	let foreign_asset_reserve_data =
		ForeignAssetReserveData { reserve: reserve_location, teleportable: false };
	let foreign_asset_id_minimum_balance = 1_000_000_000;
	// sovereign account as foreign asset owner (can be whoever for this scenario)
	let foreign_asset_owner = LocationToAccountId::convert_location(&Location::parent()).unwrap();
	let foreign_asset_create_params = (
		foreign_asset_owner.clone(),
		foreign_asset_id_location.clone(),
		foreign_asset_reserve_data,
		foreign_asset_id_minimum_balance,
	);
	let pool_params =
		(foreign_asset_owner, foreign_asset_id_location.clone(), foreign_asset_id_minimum_balance);

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
		// receiving ROCs
		foreign_asset_create_params,
		1000000000000,
		|| {
			asset_test_utils::test_cases::setup_pool_for_paying_fees_with_foreign_assets::<Runtime, RuntimeOrigin>(ExistentialDeposit::get(), pool_params);
			bridging_to_asset_hub_rococo()
		},
		(
			[PalletInstance(bp_bridge_hub_westend::WITH_BRIDGE_WESTEND_TO_ROCOCO_MESSAGES_PALLET_INDEX)].into(),
			GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
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
fn report_bridge_status_from_xcm_bridge_router_for_rococo_works() {
	asset_test_utils::test_cases_over_bridge::report_bridge_status_from_xcm_bridge_router_works::<
		Runtime,
		AllPalletsWithoutSystem,
		XcmConfig,
		LocationToAccountId,
		ToRococoXcmRouterInstance,
	>(
		collator_session_keys(),
		bridging_to_asset_hub_rococo,
		|| bp_asset_hub_westend::build_congestion_message(Default::default(), true).into(),
		|| bp_asset_hub_westend::build_congestion_message(Default::default(), false).into(),
	)
}

#[test]
fn test_report_bridge_status_call_compatibility() {
	// if this test fails, make sure `bp_asset_hub_rococo` has valid encoding
	assert_eq!(
		RuntimeCall::ToRococoXcmRouter(pallet_xcm_bridge_hub_router::Call::report_bridge_status {
			bridge_id: Default::default(),
			is_congested: true,
		})
		.encode(),
		bp_asset_hub_westend::Call::ToRococoXcmRouter(
			bp_asset_hub_westend::XcmBridgeHubRouterCall::report_bridge_status {
				bridge_id: Default::default(),
				is_congested: true,
			}
		)
		.encode()
	)
}

#[test]
fn check_sane_weight_report_bridge_status() {
	use pallet_xcm_bridge_hub_router::WeightInfo;
	let actual = <Runtime as pallet_xcm_bridge_hub_router::Config<
		ToRococoXcmRouterInstance,
	>>::WeightInfo::report_bridge_status();
	let max_weight = bp_asset_hub_westend::XcmBridgeHubRouterTransactCallMaxWeight::get();
	assert!(
		actual.all_lte(max_weight),
		"max_weight: {:?} should be adjusted to actual {:?}",
		max_weight,
		actual
	);
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
			description: "Describe Rococo Location",
			location: Location::new(2, [GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH))]),
			expected_account_id_str: "5FfpYGrFybJXFsQk7dabr1vEbQ5ycBBu85vrDjPJsF3q4A8P",
		},
		TestCase {
			description: "Describe Rococo AccountID",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					AccountId32 { network: None, id: AccountId::from(ALICE).into() },
				],
			),
			expected_account_id_str: "5CXVYinTeQKQGWAP9RqaPhitk7ybrqBZf66kCJmtAjV4Xwbg",
		},
		TestCase {
			description: "Describe Rococo AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					AccountKey20 { network: None, key: [0u8; 20] },
				],
			),
			expected_account_id_str: "5GbRhbJWb2hZY7TCeNvTqZXaP3x3UY5xt4ccxpV1ZtJS1gFL",
		},
		TestCase {
			description: "Describe Rococo Treasury Plurality",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Plurality { id: BodyId::Treasury, part: BodyPart::Voice },
				],
			),
			expected_account_id_str: "5EGi9NgJNGoMawY8ubnCDLmbdEW6nt2W2U2G3j9E3jXmspT7",
		},
		TestCase {
			description: "Describe Rococo Parachain Location",
			location: Location::new(
				2,
				[GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)), Parachain(1000)],
			),
			expected_account_id_str: "5CQeLKM7XC1xNBiQLp26Wa948cudjYRD5VzvaTG3BjnmUvLL",
		},
		TestCase {
			description: "Describe Rococo Parachain AccountID",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					AccountId32 { network: None, id: AccountId::from(ALICE).into() },
				],
			),
			expected_account_id_str: "5H8HsK17dV7i7J8fZBNd438rvwd7rHviZxJqyZpLEGJn6vb6",
		},
		TestCase {
			description: "Describe Rococo Parachain AccountKey",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					AccountKey20 { network: None, key: [0u8; 20] },
				],
			),
			expected_account_id_str: "5G121Rtddxn6zwMD2rZZGXxFHZ2xAgzFUgM9ki4A8wMGo4e2",
		},
		TestCase {
			description: "Describe Rococo Parachain Treasury Plurality",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					Plurality { id: BodyId::Treasury, part: BodyPart::Voice },
				],
			),
			expected_account_id_str: "5FNk7za2pQ71NHnN1jA63hJxJwdQywiVGnK6RL3nYjCdkWDF",
		},
		TestCase {
			description: "Describe Rococo USDT Location",
			location: Location::new(
				2,
				[
					GlobalConsensus(ByGenesis(ROCOCO_GENESIS_HASH)),
					Parachain(1000),
					PalletInstance(50),
					GeneralIndex(1984),
				],
			),
			expected_account_id_str: "5HNfT779KHeAL7PaVBTQDVxrT6dfJZJoQMTScxLSahBc9kxF",
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
	>(ExistentialDeposit::get(), ROCOCO_GENESIS_HASH);
}

#[test]
fn governance_authorize_upgrade_works() {
	use westend_runtime_constants::system_parachain::COLLECTIVES_ID;

	// no - random para
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::Location(Location::new(1, Parachain(12334)))),
		Either::Right(InstructionError { index: 0, error: XcmError::Barrier })
	);
	// ok - AssetHub (itself)
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Origin(RuntimeOrigin::root())));
	// no - Collectives
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::Location(Location::new(1, Parachain(COLLECTIVES_ID)))),
		Either::Right(InstructionError { index: 1, error: XcmError::BadOrigin })
	);
	// no - Collectives Voice of Fellows plurality
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::LocationAndDescendOrigin(
			Location::new(1, Parachain(COLLECTIVES_ID)),
			Plurality { id: BodyId::Technical, part: BodyPart::Voice }.into()
		)),
		Either::Right(InstructionError { index: 2, error: XcmError::BadOrigin })
	);

	// ok - relaychain
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Location(Location::parent())));
}

#[test]
fn weight_of_message_increases_when_dealing_with_erc20s() {
	use xcm::VersionedXcm;
	use xcm_runtime_apis::fees::runtime_decl_for_xcm_payment_api::XcmPaymentApiV2;
	let message = Xcm::<()>::builder_unsafe().withdraw_asset((Parent, 100u128)).build();
	let versioned = VersionedXcm::<()>::V5(message);
	let regular_asset_weight = Runtime::query_xcm_weight(versioned).unwrap();

	let message = Xcm::<()>::builder_unsafe()
		.withdraw_asset((AccountKey20 { network: None, key: [1u8; 20] }, 100u128))
		.build();
	let versioned = VersionedXcm::<()>::V5(message);
	let weight = Runtime::query_xcm_weight(versioned).unwrap();
	assert!(
		weight.ref_time() > regular_asset_weight.ref_time()
			// The proof size really blows up.
			&& weight.proof_size() > 10 * regular_asset_weight.proof_size()
	);
	assert_eq!(weight, crate::xcm_config::ERC20TransferGasLimit::get());
}

#[test]
fn withdraw_and_deposit_erc20s() {
	let sender: AccountId = ALICE.into();
	let beneficiary: AccountId = BOB.into();
	let revive_account = pallet_revive::Pallet::<Runtime>::account_id();
	let checking_account =
		asset_hub_westend_runtime::xcm_config::ERC20TransfersCheckingAccount::get();
	let initial_wnd_amount = 100_000_000_000_000_000u128;
	sp_tracing::init_for_tests();

	ExtBuilder::<Runtime>::default().build().execute_with(|| {
		// Bring the revive account to life.
		assert_ok!(Balances::mint_into(&revive_account, initial_wnd_amount));
		// Fund all accounts involved.
		assert_ok!(Balances::mint_into(&sender, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&beneficiary, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&checking_account, initial_wnd_amount));

		let code = compile_module_with_type("MyToken", FixtureType::Resolc)
			.expect("compile ERC20")
			.0;

		let initial_amount_u256 = U256::from(1_000_000_000_000u128);
		let constructor_data = sol_data::Uint::<256>::abi_encode(&initial_amount_u256);
		let Contract { addr: erc20_address, .. } = bare_instantiate(&sender, code)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::from_parts(500_000_000_000, 10 * 1024 * 1024),
				deposit_limit: Balance::MAX,
			})
			.data(constructor_data)
			.build_and_unwrap_contract();

		let sender_balance_before = <Balances as fungible::Inspect<_>>::balance(&sender);

		let erc20_transfer_amount = 100u128;
		let wnd_amount_for_fees = 10_000_000_000_000u128;
		// Actual XCM to execute locally.
		let message = Xcm::<RuntimeCall>::builder()
			.withdraw_asset((Parent, wnd_amount_for_fees))
			.pay_fees((Parent, wnd_amount_for_fees))
			.withdraw_asset((
				AccountKey20 { key: erc20_address.into(), network: None },
				erc20_transfer_amount,
			))
			.deposit_asset(AllCounted(1), beneficiary.clone())
			.refund_surplus()
			.deposit_asset(AllCounted(1), sender.clone())
			.build();
		assert_ok!(PolkadotXcm::execute(
			RuntimeOrigin::signed(sender.clone()),
			Box::new(VersionedXcm::V5(message)),
			Weight::from_parts(600_000_000_000, 15 * 1024 * 1024),
		));

		// Revive is not taking any fees.
		let sender_balance_after = <Balances as fungible::Inspect<_>>::balance(&sender);
		// Balance after is larger than the difference between balance before and transferred
		// amount because of the refund.
		assert!(sender_balance_after > sender_balance_before - wnd_amount_for_fees);

		// Beneficiary receives the ERC20.
		let beneficiary_amount =
			<Revive as fungibles::Inspect<_>>::balance(erc20_address, &beneficiary);
		assert_eq!(beneficiary_amount, erc20_transfer_amount);
	});
}

#[test]
fn non_existent_erc20_will_error() {
	let sender: AccountId = ALICE.into();
	let beneficiary: AccountId = BOB.into();
	let revive_account = pallet_revive::Pallet::<Runtime>::account_id();
	let checking_account =
		asset_hub_westend_runtime::xcm_config::ERC20TransfersCheckingAccount::get();
	let initial_wnd_amount = 10_000_000_000_000u128;
	// We try to withdraw an ERC20 token but the address doesn't exist.
	let non_existent_contract_address = [1u8; 20];

	ExtBuilder::<Runtime>::default().build().execute_with(|| {
		// Bring the revive account to life.
		assert_ok!(Balances::mint_into(&revive_account, initial_wnd_amount));
		// Fund all accounts involved.
		assert_ok!(Balances::mint_into(&sender, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&beneficiary, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&checking_account, initial_wnd_amount));

		let wnd_amount_for_fees = 1_000_000_000_000u128;
		let erc20_transfer_amount = 100u128;
		let message = Xcm::<RuntimeCall>::builder()
			.withdraw_asset((Parent, wnd_amount_for_fees))
			.pay_fees((Parent, wnd_amount_for_fees))
			.withdraw_asset((
				AccountKey20 { key: non_existent_contract_address, network: None },
				erc20_transfer_amount,
			))
			.deposit_asset(AllCounted(1), beneficiary.clone())
			.build();
		// Execution fails but doesn't panic.
		assert!(PolkadotXcm::execute(
			RuntimeOrigin::signed(sender.clone()),
			Box::new(VersionedXcm::V5(message)),
			Weight::from_parts(2_500_000_000, 120_000),
		)
		.is_err());
	});
}

#[test]
fn smart_contract_not_erc20_will_error() {
	let sender: AccountId = ALICE.into();
	let beneficiary: AccountId = BOB.into();
	let revive_account = pallet_revive::Pallet::<Runtime>::account_id();
	let checking_account =
		asset_hub_westend_runtime::xcm_config::ERC20TransfersCheckingAccount::get();
	let initial_wnd_amount = 10_000_000_000_000u128;

	ExtBuilder::<Runtime>::default().build().execute_with(|| {
		// Bring the revive account to life.
		assert_ok!(Balances::mint_into(&revive_account, initial_wnd_amount));

		// Fund all accounts involved.
		assert_ok!(Balances::mint_into(&sender, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&beneficiary, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&checking_account, initial_wnd_amount));

		let (code, _) = compile_module("dummy").unwrap();

		let Contract { addr: non_erc20_address, .. } = bare_instantiate(&sender, code)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::from_parts(500_000_000_000, 10 * 1024 * 1024),
				deposit_limit: Balance::MAX,
			})
			.build_and_unwrap_contract();

		let wnd_amount_for_fees = 1_000_000_000_000u128;
		let erc20_transfer_amount = 100u128;
		let message = Xcm::<RuntimeCall>::builder()
			.withdraw_asset((Parent, wnd_amount_for_fees))
			.pay_fees((Parent, wnd_amount_for_fees))
			.withdraw_asset((
				AccountKey20 { key: non_erc20_address.into(), network: None },
				erc20_transfer_amount,
			))
			.deposit_asset(AllCounted(1), beneficiary.clone())
			.build();
		// Execution fails but doesn't panic.
		assert!(PolkadotXcm::execute(
			RuntimeOrigin::signed(sender.clone()),
			Box::new(VersionedXcm::V5(message)),
			Weight::from_parts(2_500_000_000, 120_000),
		)
		.is_err());
	});
}

// Here the contract returns a number but because it can be cast to true
// it still succeeds.
#[test]
fn smart_contract_does_not_return_bool_fails() {
	let sender: AccountId = ALICE.into();
	let beneficiary: AccountId = BOB.into();
	let revive_account = pallet_revive::Pallet::<Runtime>::account_id();
	let checking_account =
		asset_hub_westend_runtime::xcm_config::ERC20TransfersCheckingAccount::get();
	let initial_wnd_amount = 10_000_000_000_000u128;

	ExtBuilder::<Runtime>::default().build().execute_with(|| {
		// Bring the revive account to life.
		assert_ok!(Balances::mint_into(&revive_account, initial_wnd_amount));

		// Fund all accounts involved.
		assert_ok!(Balances::mint_into(&sender, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&beneficiary, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&checking_account, initial_wnd_amount));

		// This contract implements the ERC20 interface for `transfer` except it returns a uint256.
		let code = compile_module_with_type("MyTokenFake", FixtureType::Resolc)
			.expect("compile ERC20")
			.0;

		let initial_amount_u256 = U256::from(1_000_000_000_000u128);
		let constructor_data = sol_data::Uint::<256>::abi_encode(&initial_amount_u256);

		let Contract { addr: non_erc20_address, .. } = bare_instantiate(&sender, code)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::from_parts(500_000_000_000, 10 * 1024 * 1024),
				deposit_limit: Balance::MAX,
			})
			.data(constructor_data)
			.build_and_unwrap_contract();

		let wnd_amount_for_fees = 1_000_000_000_000u128;
		let erc20_transfer_amount = 100u128;
		let message = Xcm::<RuntimeCall>::builder()
			.withdraw_asset((Parent, wnd_amount_for_fees))
			.pay_fees((Parent, wnd_amount_for_fees))
			.withdraw_asset((
				AccountKey20 { key: non_erc20_address.into(), network: None },
				erc20_transfer_amount,
			))
			.deposit_asset(AllCounted(1), beneficiary.clone())
			.build();
		// Execution fails but doesn't panic.
		assert!(PolkadotXcm::execute(
			RuntimeOrigin::signed(sender.clone()),
			Box::new(VersionedXcm::V5(message)),
			Weight::from_parts(2_500_000_000, 220_000),
		)
		.is_err());
	});
}

#[test]
fn expensive_erc20_runs_out_of_gas() {
	let sender: AccountId = ALICE.into();
	let beneficiary: AccountId = BOB.into();
	let revive_account = pallet_revive::Pallet::<Runtime>::account_id();
	let checking_account =
		asset_hub_westend_runtime::xcm_config::ERC20TransfersCheckingAccount::get();
	let initial_wnd_amount = 10_000_000_000_000u128;

	ExtBuilder::<Runtime>::default().build().execute_with(|| {
		// Bring the revive account to life.
		assert_ok!(Balances::mint_into(&revive_account, initial_wnd_amount));

		// Fund all accounts involved.
		assert_ok!(Balances::mint_into(&sender, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&beneficiary, initial_wnd_amount));
		assert_ok!(Balances::mint_into(&checking_account, initial_wnd_amount));

		// This contract does a lot more storage writes in `transfer`.
		let code = compile_module_with_type("MyTokenExpensive", FixtureType::Resolc)
			.expect("compile ERC20")
			.0;

		let initial_amount_u256 = U256::from(1_000_000_000_000u128);
		let constructor_data = sol_data::Uint::<256>::abi_encode(&initial_amount_u256);
		let Contract { addr: non_erc20_address, .. } = bare_instantiate(&sender, code)
			.transaction_limits(TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::from_parts(500_000_000_000, 10 * 1024 * 1024),
				deposit_limit: Balance::MAX,
			})
			.data(constructor_data)
			.build_and_unwrap_contract();

		let wnd_amount_for_fees = 1_000_000_000_000u128;
		let erc20_transfer_amount = 100u128;
		let message = Xcm::<RuntimeCall>::builder()
			.withdraw_asset((Parent, wnd_amount_for_fees))
			.pay_fees((Parent, wnd_amount_for_fees))
			.withdraw_asset((
				AccountKey20 { key: non_erc20_address.into(), network: None },
				erc20_transfer_amount,
			))
			.deposit_asset(AllCounted(1), beneficiary.clone())
			.build();
		// Execution fails but doesn't panic.
		assert!(PolkadotXcm::execute(
			RuntimeOrigin::signed(sender.clone()),
			Box::new(VersionedXcm::V5(message)),
			Weight::from_parts(2_500_000_000, 120_000),
		)
		.is_err());
	});
}

#[test]
fn staking_inflation_correct_single_era() {
	let total = staking::IssuanceCurve::issue(0, MILLISECONDS_PER_HOUR);
	// Total per hour is ~47.6 WND
	assert_relative_eq!(total as f64, (4_760 * CENTS) as f64, max_relative = 0.001);
}

#[test]
fn staking_inflation_correct_longer_era() {
	// Twice the era duration means twice the emission:
	let total_1x = staking::IssuanceCurve::issue(0, MILLISECONDS_PER_HOUR);
	let total_2x = staking::IssuanceCurve::issue(0, 2 * MILLISECONDS_PER_HOUR);
	assert_relative_eq!(total_2x as f64, total_1x as f64 * 2.0, max_relative = 0.001);
}

#[test]
fn staking_inflation_correct_whole_year() {
	let yearly_emission =
		staking::IssuanceCurve::issue(0, (36525 * 24 * MILLISECONDS_PER_HOUR) / 100);
	// Our yearly emissions is about 417k WND:
	assert_relative_eq!(yearly_emission as f64, (417_307 * UNITS) as f64, max_relative = 0.001);
}

// 10 years into the future, our values do not overflow.
#[test]
fn staking_inflation_correct_not_overflow() {
	let ten_year_emission =
		staking::IssuanceCurve::issue(0, (36525 * 24 * MILLISECONDS_PER_HOUR) / 10);
	let initial_ti: i128 = 5_216_342_402_773_185_773;
	let projected_total_issuance = ten_year_emission as i128 + initial_ti;

	// In 2034, there will be about 9.39 million WND in existence.
	assert_relative_eq!(
		projected_total_issuance as f64,
		(9_390_000 * UNITS) as f64,
		max_relative = 0.001
	);
}

// Print percent per year, just as convenience.
#[test]
fn staking_inflation_correct_print_percent() {
	let yearly_emission =
		staking::IssuanceCurve::issue(0, (36525 * 24 * MILLISECONDS_PER_HOUR) / 100);
	let mut ti: i128 = 5_216_342_402_773_185_773;

	for y in 0..10 {
		let new_ti = ti + yearly_emission as i128;
		let inflation = 100.0 * (new_ti - ti) as f64 / ti as f64;
		println!("Year {y} inflation: {inflation}%");
		ti = new_ti;

		assert!(inflation <= 8.0 && inflation > 2.0, "sanity check");
	}
}

#[test]
fn exchange_asset_success() {
	exchange_asset_on_asset_hub_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		ForeignAssetsInstance,
	>(
		collator_session_keys(),
		ASSET_HUB_ID,
		AccountId::from(ALICE),
		WestendLocation::get(),
		true,
		500 * UNITS,
		665 * UNITS,
		None,
	);
}

#[test]
fn exchange_asset_insufficient_liquidity() {
	let log_capture = capture_test_logs!({
		exchange_asset_on_asset_hub_works::<
			Runtime,
			RuntimeCall,
			RuntimeOrigin,
			Block,
			ForeignAssetsInstance,
		>(
			collator_session_keys(),
			ASSET_HUB_ID,
			AccountId::from(ALICE),
			WestendLocation::get(),
			true,
			1_000 * UNITS,
			2_000 * UNITS,
			Some(xcm::v5::InstructionError { index: 1, error: xcm::v5::Error::NoDeal }),
		);
	});
	assert!(log_capture.contains("NoDeal"));
}

#[test]
fn exchange_asset_insufficient_balance() {
	let log_capture = capture_test_logs!({
		exchange_asset_on_asset_hub_works::<
			Runtime,
			RuntimeCall,
			RuntimeOrigin,
			Block,
			ForeignAssetsInstance,
		>(
			collator_session_keys(),
			ASSET_HUB_ID,
			AccountId::from(ALICE),
			WestendLocation::get(),
			true,
			5_000 * UNITS, // This amount will be greater than initial balance
			1_665 * UNITS,
			Some(xcm::v5::InstructionError {
				index: 0,
				error: xcm::v5::Error::FailedToTransactAsset(""),
			}),
		);
	});
	assert!(log_capture.contains("Funds are unavailable"));
}

#[test]
fn exchange_asset_pool_not_created() {
	exchange_asset_on_asset_hub_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		ForeignAssetsInstance,
	>(
		collator_session_keys(),
		ASSET_HUB_ID,
		AccountId::from(ALICE),
		WestendLocation::get(),
		false, // Pool not created
		500 * UNITS,
		665 * UNITS,
		Some(xcm::v5::InstructionError { index: 1, error: xcm::v5::Error::NoDeal }),
	);
}

#[test]
fn exchange_asset_from_penpal_via_asset_hub_back_to_penpal() {
	exchange_asset_on_asset_hub_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		ForeignAssetsInstance,
	>(
		collator_session_keys(),
		ASSET_HUB_ID,
		AccountId::from(ALICE),
		WestendLocation::get(),
		true,
		100_000_000_000u128,
		1_000_000_000u128,
		None,
	);
}

/// Verify that AssetHub's `RelayChainSessionKeys` is compatible with Westend's `SessionKeys`.
#[test]
fn session_keys_are_compatible_between_ah_and_rc() {
	use asset_hub_westend_runtime::staking::RelayChainSessionKeys;

	// Verify the key type IDs match in order.
	// This ensures that when keys are encoded on AssetHub and decoded on Westend (or vice versa),
	// they map to the correct key types.
	assert_eq!(
		RelayChainSessionKeys::key_ids(),
		westend_runtime::SessionKeys::key_ids(),
		"Session key type IDs must match between AssetHub and Westend"
	);
}

#[test]
fn staking_proxy_can_manage_staking_operator() {
	use asset_hub_westend_runtime::ProxyType;
	use frame_support::traits::InstanceFilter;

	// GIVEN: Staking proxy type
	let staking_proxy = ProxyType::Staking;

	// WHEN: checking if Staking can add/remove StakingOperator proxies
	let add_call = RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
		delegate: AccountId::from(BOB).into(),
		proxy_type: ProxyType::StakingOperator,
		delay: 0,
	});
	let remove_call = RuntimeCall::Proxy(pallet_proxy::Call::remove_proxy {
		delegate: AccountId::from(BOB).into(),
		proxy_type: ProxyType::StakingOperator,
		delay: 0,
	});

	// THEN: Staking proxy can manage StakingOperator proxies and is its superset
	assert!(staking_proxy.filter(&add_call));
	assert!(staking_proxy.filter(&remove_call));
	assert!(staking_proxy.is_superset(&ProxyType::StakingOperator));
}

/// Every `ProxyType` variant.
///
/// Listed exhaustively (rather than derived) so that adding a variant forces this list to be
/// updated, keeping [`proxy_type_superset_relation_matches_call_filters`] complete.
fn all_proxy_types() -> Vec<asset_hub_westend_runtime::ProxyType> {
	use asset_hub_westend_runtime::ProxyType;

	let all = vec![
		ProxyType::Any,
		ProxyType::NonTransfer,
		ProxyType::CancelProxy,
		ProxyType::Assets,
		ProxyType::AssetOwner,
		ProxyType::AssetManager,
		ProxyType::Collator,
		ProxyType::Governance,
		ProxyType::Staking,
		ProxyType::NominationPools,
		ProxyType::OldSudoBalances,
		ProxyType::OldIdentityJudgement,
		ProxyType::OldAuction,
		ProxyType::OldParaRegistration,
		ProxyType::StakingOperator,
	];

	// Exhaustiveness guard: a new variant added to `ProxyType` breaks this `match`, which is the
	// signal to also add it above.
	for proxy_type in all.iter() {
		match proxy_type {
			ProxyType::Any |
			ProxyType::NonTransfer |
			ProxyType::CancelProxy |
			ProxyType::Assets |
			ProxyType::AssetOwner |
			ProxyType::AssetManager |
			ProxyType::Collator |
			ProxyType::Governance |
			ProxyType::Staking |
			ProxyType::NominationPools |
			ProxyType::OldSudoBalances |
			ProxyType::OldIdentityJudgement |
			ProxyType::OldAuction |
			ProxyType::OldParaRegistration |
			ProxyType::StakingOperator => (),
		}
	}

	all
}

/// At least one call from every pallet family referenced by the `InstanceFilter<RuntimeCall>`
/// implementation for `ProxyType`, so that the lattice check below actually exercises the
/// boundaries the filters draw.
fn representative_proxy_calls() -> Vec<RuntimeCall> {
	let remark = || RuntimeCall::System(frame_system::Call::remark { remark: vec![] });

	vec![
		remark(),
		RuntimeCall::Balances(pallet_balances::Call::transfer_keep_alive {
			dest: AccountId::from(BOB).into(),
			value: 1,
		}),
		// Admitted by `AssetOwner`.
		RuntimeCall::Assets(pallet_assets::Call::<Runtime, TrustBackedAssetsInstance>::create {
			id: codec::Compact(1),
			admin: AccountId::from(BOB).into(),
			min_balance: 1,
		}),
		// Admitted by `AssetManager`.
		RuntimeCall::Assets(pallet_assets::Call::<Runtime, TrustBackedAssetsInstance>::mint {
			id: codec::Compact(1),
			beneficiary: AccountId::from(BOB).into(),
			amount: 1,
		}),
		// Admitted by neither `AssetOwner` nor `AssetManager`, only by `Assets`.
		RuntimeCall::Assets(pallet_assets::Call::<Runtime, TrustBackedAssetsInstance>::transfer {
			id: codec::Compact(1),
			target: AccountId::from(BOB).into(),
			amount: 1,
		}),
		RuntimeCall::Nfts(pallet_nfts::Call::set_collection_max_supply {
			collection: 1,
			max_supply: 1,
		}),
		RuntimeCall::Nfts(pallet_nfts::Call::lock_item_transfer { collection: 1, item: 1 }),
		RuntimeCall::Uniques(pallet_uniques::Call::set_collection_max_supply {
			collection: 1,
			max_supply: 1,
		}),
		RuntimeCall::Uniques(pallet_uniques::Call::freeze { collection: 1, item: 1 }),
		RuntimeCall::NftFractionalization(pallet_nft_fractionalization::Call::unify {
			nft_collection_id: 1,
			nft_id: 1,
			asset_id: 1,
			beneficiary: AccountId::from(BOB).into(),
		}),
		RuntimeCall::Scheduler(pallet_scheduler::Call::cancel { when: 1, index: 0 }),
		RuntimeCall::Treasury(pallet_treasury::Call::void_spend { index: 0 }),
		RuntimeCall::Vesting(pallet_vesting::Call::vest {}),
		RuntimeCall::ConvictionVoting(pallet_conviction_voting::Call::remove_vote {
			class: None,
			index: 0,
		}),
		RuntimeCall::Referenda(pallet_referenda::Call::refund_decision_deposit { index: 0 }),
		RuntimeCall::Whitelist(pallet_whitelist::Call::remove_whitelisted_call {
			call_hash: Default::default(),
		}),
		RuntimeCall::Proxy(pallet_proxy::Call::reject_announcement {
			delegate: AccountId::from(BOB).into(),
			call_hash: Default::default(),
		}),
		RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
			delegate: AccountId::from(BOB).into(),
			proxy_type: asset_hub_westend_runtime::ProxyType::StakingOperator,
			delay: 0,
		}),
		RuntimeCall::Utility(pallet_utility::Call::batch { calls: vec![] }),
		RuntimeCall::Utility(pallet_utility::Call::as_derivative {
			index: 0,
			call: Box::new(remark()),
		}),
		RuntimeCall::Multisig(pallet_multisig::Call::as_multi_threshold_1 {
			other_signatories: vec![],
			call: Box::new(remark()),
		}),
		RuntimeCall::CollatorSelection(pallet_collator_selection::Call::set_desired_candidates {
			max: 1,
		}),
		RuntimeCall::Session(pallet_session::Call::purge_keys {}),
		RuntimeCall::Staking(pallet_staking_async::Call::chill {}),
		RuntimeCall::Staking(pallet_staking_async::Call::nominate { targets: vec![] }),
		RuntimeCall::StakingRcClient(pallet_staking_async_rc_client::Call::purge_keys {
			max_delivery_and_remote_execution_fee: None,
		}),
		RuntimeCall::NominationPools(pallet_nomination_pools::Call::chill { pool_id: 0 }),
		RuntimeCall::VoterList(
			pallet_bags_list::Call::<Runtime, pallet_bags_list::Instance1>::rebag {
				dislocated: AccountId::from(BOB).into(),
			},
		),
	]
}

/// `pallet_proxy` authorizes `add_proxy`/`remove_proxy` through `ProxyType::is_superset`, so a
/// proxy type that *declares* itself a superset of another must also *admit* every call that other
/// type admits. Otherwise the "smaller" type is reachable as an escalation: the declared superset
/// can grant itself the subset proxy and thereby gain permissions its own filter denies.
///
/// This checks that property across the whole lattice rather than a single pair, so the class of
/// bug cannot silently reappear on another edge.
#[test]
fn proxy_type_superset_relation_matches_call_filters() {
	use frame_support::traits::InstanceFilter;

	let calls = representative_proxy_calls();

	for superset in all_proxy_types() {
		for subset in all_proxy_types() {
			if !superset.is_superset(&subset) {
				continue;
			}

			for call in calls.iter() {
				if subset.filter(call) {
					assert!(
						superset.filter(call),
						"lattice violated: {superset:?} declares itself a superset of {subset:?}, \
						 but rejects {call:?} which {subset:?} admits",
					);
				}
			}
		}
	}
}

/// Regression test for <https://github.com/paritytech/polkadot-sdk/issues/12724>.
///
/// `NonTransfer` used to claim `Governance` as a subset while denying the `Treasury`,
/// `ConvictionVoting`, `Referenda` and `Whitelist` calls that `Governance` admits, which let a
/// `NonTransfer` proxy add a `Governance` proxy and widen its own permissions.
#[test]
fn non_transfer_proxy_is_not_a_superset_of_governance() {
	use asset_hub_westend_runtime::ProxyType;
	use frame_support::traits::InstanceFilter;

	// `NonTransfer` denies every governance call family that `Governance` admits, except `Utility`.
	for call in [
		RuntimeCall::Treasury(pallet_treasury::Call::void_spend { index: 0 }),
		RuntimeCall::ConvictionVoting(pallet_conviction_voting::Call::remove_vote {
			class: None,
			index: 0,
		}),
		RuntimeCall::Referenda(pallet_referenda::Call::refund_decision_deposit { index: 0 }),
		RuntimeCall::Whitelist(pallet_whitelist::Call::remove_whitelisted_call {
			call_hash: Default::default(),
		}),
	] {
		assert!(ProxyType::Governance.filter(&call), "Governance must admit {call:?}");
		assert!(!ProxyType::NonTransfer.filter(&call), "NonTransfer must deny {call:?}");
	}

	// So it must not declare itself a superset of it. This is what stops a `NonTransfer` proxy
	// from granting itself a `Governance` proxy: `pallet_proxy` gates `add_proxy`/`remove_proxy`
	// on `is_superset` before it ever consults `filter`, which by itself does not deny `Proxy`
	// calls.
	assert!(!ProxyType::NonTransfer.is_superset(&ProxyType::Governance));

	// The other declared `NonTransfer` subsets are unaffected.
	for subset in [
		ProxyType::Collator,
		ProxyType::Staking,
		ProxyType::NominationPools,
		ProxyType::StakingOperator,
	] {
		assert!(ProxyType::NonTransfer.is_superset(&subset));
	}
}

/// A location usable as an asset id for the `xcm::v5::Location`-keyed pallets.
fn some_asset_location() -> xcm::v5::Location {
	xcm::v5::Location::new(1, [xcm::v5::Junction::Parachain(1000)])
}

/// Calls that move the delegator's funds or assets, and so must be denied to a `NonTransfer`
/// proxy. Each is labelled so a failure names the call that slipped through.
fn value_moving_calls() -> Vec<(&'static str, RuntimeCall)> {
	vec![
		(
			"ForeignAssets::transfer",
			RuntimeCall::ForeignAssets(
				pallet_assets::Call::<Runtime, ForeignAssetsInstance>::transfer {
					id: some_asset_location(),
					target: AccountId::from(BOB).into(),
					amount: 1,
				},
			),
		),
		(
			"PoolAssets::transfer",
			RuntimeCall::PoolAssets(pallet_assets::Call::<
				Runtime,
				asset_hub_westend_runtime::PoolAssetsInstance,
			>::transfer {
				id: 1,
				target: AccountId::from(BOB).into(),
				amount: 1,
			}),
		),
		(
			"AssetConversion::swap_exact_tokens_for_tokens",
			RuntimeCall::AssetConversion(
				pallet_asset_conversion::Call::swap_exact_tokens_for_tokens {
					path: vec![Box::new(some_asset_location()), Box::new(some_asset_location())],
					amount_in: 1,
					amount_out_min: 1,
					send_to: AccountId::from(BOB),
					keep_alive: false,
				},
			),
		),
		(
			"Psm::mint",
			RuntimeCall::Psm(pallet_psm::Call::mint {
				internal_asset: some_asset_location(),
				external_asset: some_asset_location(),
				external_amount: 1,
				max_fee: sp_runtime::Permill::zero(),
			}),
		),
		(
			"PolkadotXcm::transfer_assets",
			RuntimeCall::PolkadotXcm(pallet_xcm::Call::transfer_assets {
				dest: Box::new(xcm::VersionedLocation::from(some_asset_location())),
				beneficiary: Box::new(xcm::VersionedLocation::from(some_asset_location())),
				assets: Box::new(xcm::VersionedAssets::from(XcmAssets::new())),
				fee_asset_item: 0,
				weight_limit: WeightLimit::Unlimited,
			}),
		),
		(
			"Revive::call",
			RuntimeCall::Revive(pallet_revive::Call::call {
				dest: Default::default(),
				value: 1,
				weight_limit: Weight::zero(),
				storage_deposit_limit: 0,
				data: vec![],
			}),
		),
		(
			"Indices::transfer",
			RuntimeCall::Indices(pallet_indices::Call::transfer {
				new: AccountId::from(BOB).into(),
				index: 0,
			}),
		),
	]
}

/// Regression test for <https://github.com/paritytech/polkadot-sdk/issues/12466>.
///
/// `ProxyType::NonTransfer` is documented as permitting "any call that does not transfer funds or
/// assets", but it is implemented as a deny-list and so fails open. `ForeignAssets` and
/// `PoolAssets` transfers were reachable, as were swaps, XCM transfers, contract calls carrying a
/// value, and index transfers (which repatriate the reserved deposit).
#[test]
fn non_transfer_proxy_rejects_value_moving_calls() {
	use asset_hub_westend_runtime::ProxyType;
	use frame_support::traits::InstanceFilter;

	// Collected rather than asserted one by one, so a regression reports every call that slipped
	// through instead of only the first.
	let mut leaked = Vec::new();
	for (name, call) in value_moving_calls() {
		if ProxyType::NonTransfer.filter(&call) {
			leaked.push(name);
		}
		// The call is otherwise well-formed and reachable by a fully permissioned proxy.
		assert!(ProxyType::Any.filter(&call), "Any must permit {name}");
	}

	assert!(
		leaked.is_empty(),
		"NonTransfer must reject calls that move funds or assets, but permitted: {leaked:?}",
	);
}

/// The deny-list above must not over-reach: calls that do not move value stay available, including
/// the ones backing the proxy types `NonTransfer` declares as its subsets.
#[test]
fn non_transfer_proxy_still_permits_non_value_moving_calls() {
	use asset_hub_westend_runtime::ProxyType;
	use frame_support::traits::InstanceFilter;

	let permitted = vec![
		("Indices::claim", RuntimeCall::Indices(pallet_indices::Call::claim { index: 0 })),
		("Indices::freeze", RuntimeCall::Indices(pallet_indices::Call::freeze { index: 0 })),
		("Vesting::vest", RuntimeCall::Vesting(pallet_vesting::Call::vest {})),
		("Staking::chill", RuntimeCall::Staking(pallet_staking_async::Call::chill {})),
		("Session::purge_keys", RuntimeCall::Session(pallet_session::Call::purge_keys {})),
		(
			"NominationPools::chill",
			RuntimeCall::NominationPools(pallet_nomination_pools::Call::chill { pool_id: 0 }),
		),
		(
			"CollatorSelection::set_desired_candidates",
			RuntimeCall::CollatorSelection(
				pallet_collator_selection::Call::set_desired_candidates { max: 1 },
			),
		),
		("System::remark", RuntimeCall::System(frame_system::Call::remark { remark: vec![] })),
	];

	for (name, call) in permitted {
		assert!(ProxyType::NonTransfer.filter(&call), "NonTransfer must still permit {name}");
	}
}

/// Verifies StakingOperator filter allows validator operations and session key management,
/// but forbids fund management.
#[test]
fn staking_operator_filter_allows_validator_ops_and_session_keys() {
	use asset_hub_westend_runtime::ProxyType;
	use frame_support::traits::InstanceFilter;
	use pallet_staking_async::{Call as StakingCall, RewardDestination, ValidatorPrefs};
	use pallet_staking_async_rc_client::Call as RcClientCall;

	let operator = ProxyType::StakingOperator;

	// StakingOperator can perform validator operations
	assert!(operator
		.filter(&RuntimeCall::Staking(StakingCall::validate { prefs: ValidatorPrefs::default() })));
	assert!(operator.filter(&RuntimeCall::Staking(StakingCall::chill {})));
	assert!(operator.filter(&RuntimeCall::Staking(StakingCall::kick { who: vec![] })));

	// StakingOperator can manage session keys
	assert!(operator.filter(&RuntimeCall::StakingRcClient(RcClientCall::set_keys {
		keys: Default::default(),
		proof: Default::default(),
		max_delivery_and_remote_execution_fee: None,
	})));
	assert!(operator.filter(&RuntimeCall::StakingRcClient(RcClientCall::purge_keys {
		max_delivery_and_remote_execution_fee: None,
	})));

	// StakingOperator can batch operations
	assert!(operator.filter(&RuntimeCall::Utility(pallet_utility::Call::batch { calls: vec![] })));
	assert!(
		operator.filter(&RuntimeCall::Utility(pallet_utility::Call::batch_all { calls: vec![] }))
	);
	assert!(
		operator.filter(&RuntimeCall::Utility(pallet_utility::Call::force_batch { calls: vec![] }))
	);

	// StakingOperator cannot use non-batching utility calls
	assert!(!operator.filter(&RuntimeCall::Utility(pallet_utility::Call::as_derivative {
		index: 0,
		call: Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![] })),
	})));
	assert!(!operator.filter(&RuntimeCall::Utility(pallet_utility::Call::dispatch_as {
		as_origin: Box::new(asset_hub_westend_runtime::OriginCaller::system(
			frame_system::RawOrigin::Root,
		)),
		call: Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![] })),
	})));
	assert!(!operator.filter(&RuntimeCall::Utility(pallet_utility::Call::with_weight {
		call: Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![] })),
		weight: Default::default(),
	})));

	// StakingOperator cannot manage funds or nominations
	assert!(!operator.filter(&RuntimeCall::Staking(StakingCall::bond {
		value: 100,
		payee: RewardDestination::Staked
	})));
	assert!(!operator.filter(&RuntimeCall::Staking(StakingCall::unbond { value: 100 })));
	assert!(!operator.filter(&RuntimeCall::Staking(StakingCall::nominate { targets: vec![] })));
	assert!(!operator.filter(&RuntimeCall::Staking(StakingCall::set_payee {
		payee: RewardDestination::Staked
	})));
}

/// Test that a pure proxy stash can delegate to a StakingOperator
/// who can then call validate, chill, and manage session keys.
#[test]
fn pure_proxy_stash_can_delegate_to_staking_operator() {
	use asset_hub_westend_runtime::ProxyType;

	let controller: AccountId = ALICE.into();
	let operator: AccountId = BOB.into();

	ExtBuilder::<Runtime>::default()
		.with_collators(vec![AccountId::from(ALICE)])
		.with_session_keys(vec![(
			AccountId::from(ALICE),
			AccountId::from(ALICE),
			SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
		)])
		.build()
		.execute_with(|| {
			// GIVEN: fund controller and operator
			assert_ok!(Balances::mint_into(&controller, 100 * UNITS));
			assert_ok!(Balances::mint_into(&operator, 100 * UNITS));

			// WHEN: controller creates a pure proxy stash with Staking proxy type
			assert_ok!(Proxy::create_pure(
				RuntimeOrigin::signed(controller.clone()),
				ProxyType::Staking,
				0,
				0
			));
			let pure_stash = Proxy::pure_account(&controller, &ProxyType::Staking, 0, None);

			// Fund the pure proxy stash
			assert_ok!(Balances::mint_into(&pure_stash, 100 * UNITS));

			// WHEN: controller (via Staking proxy) adds StakingOperator proxy for the operator
			let add_operator_call = RuntimeCall::Proxy(pallet_proxy::Call::add_proxy {
				delegate: operator.clone().into(),
				proxy_type: ProxyType::StakingOperator,
				delay: 0,
			});
			assert_ok!(Proxy::proxy(
				RuntimeOrigin::signed(controller.clone()),
				pure_stash.clone().into(),
				None,
				Box::new(add_operator_call),
			));

			// THEN: operator can call chill on behalf of pure proxy stash
			let chill_call = RuntimeCall::Staking(pallet_staking_async::Call::chill {});
			assert_ok!(Proxy::proxy(
				RuntimeOrigin::signed(operator.clone()),
				pure_stash.clone().into(),
				None,
				Box::new(chill_call),
			));

			// THEN: operator can call validate on behalf of pure proxy stash
			let validate_call = RuntimeCall::Staking(pallet_staking_async::Call::validate {
				prefs: Default::default(),
			});
			assert_ok!(Proxy::proxy(
				RuntimeOrigin::signed(operator.clone()),
				pure_stash.clone().into(),
				None,
				Box::new(validate_call),
			));

			// THEN: operator can call purge_keys (session key management on AssetHub)
			let purge_keys_call =
				RuntimeCall::StakingRcClient(pallet_staking_async_rc_client::Call::purge_keys {
					max_delivery_and_remote_execution_fee: None,
				});
			assert_ok!(Proxy::proxy(
				RuntimeOrigin::signed(operator.clone()),
				pure_stash.clone().into(),
				None,
				Box::new(purge_keys_call),
			));

			// THEN: operator CANNOT call bond (fund management is forbidden)
			// Note: Proxy::proxy returns Ok(()) even when the proxied call fails due to filter.
			// The actual result is emitted as a ProxyExecuted event.
			let bond_call = RuntimeCall::Staking(pallet_staking_async::Call::bond {
				value: 10 * UNITS,
				payee: pallet_staking_async::RewardDestination::Staked,
			});
			assert_ok!(Proxy::proxy(
				RuntimeOrigin::signed(operator.clone()),
				pure_stash.clone().into(),
				None,
				Box::new(bond_call),
			));
			// Check that the proxied call failed due to filter (CallFiltered error)
			System::assert_last_event(
				pallet_proxy::Event::ProxyExecuted {
					result: Err(frame_system::Error::<Runtime>::CallFiltered.into()),
				}
				.into(),
			);
		});
}

mod remote_test {
	use super::*;

	/// Test claim_trapped_balance for all pool members using a state snapshot.
	///
	/// The test iterates through all pool members, computes trapped amounts, and calls
	/// `do_claim_trapped_balance` for those with trapped funds. Only successful claims are printed.
	///
	/// Run with:
	/// ```bash
	/// SNAP=<PATH_TO_SNAP> cargo test -r -p asset-hub-westend-runtime np_claim_trapped_balance \
	/// -- --ignored --nocapture
	/// ```
	///
	/// Note: If you want to test this with PAH snapshot, ensure (locally, DO NOT COMMIT)
	/// 1) WAH staking pallet indices align with PAH
	/// 2) WAH ED is same as PAH (decrease it by 10x in `../../../constants/src/westend.rs`)
	/// 3) Staking Bonding Duration is 28 eras.
	#[tokio::test]
	#[ignore]
	async fn np_claim_trapped_balance() {
		use pallet_nomination_pools::{Pallet as NominationPools, PoolMembers};
		use remote_externalities::{Builder, Mode, OfflineConfig, SnapshotConfig};

		let snap_path =
			std::env::var("SNAP").expect("SNAP env var not set. Please provide snapshot path.");

		println!("Loading snapshot from: {}", snap_path);

		let mut ext = Builder::<Block>::new()
			.mode(Mode::Offline(OfflineConfig { state_snapshot: SnapshotConfig::new(snap_path) }))
			.build()
			.await
			.expect("Failed to load snapshot");

		ext.execute_with(|| {
			use pallet_nomination_pools::adapter::{Member, StakeStrategy};

			const DOT_DECIMALS: u128 = 10_000_000_000; // 10 decimals for DOT

			println!("\nChecking trapped balance for all pool members...\n");

			let mut total_members = 0u32;
			let mut success_count = 0u32;
			let mut total_claimed = 0u128;

			println!("member,pool_id,trapped_dot");

			for (member_account, member_data) in PoolMembers::<Runtime>::iter() {
				total_members += 1;

				// Compute trapped amount before calling the helper
				let expected = member_data.total_balance();
				let actual = <Runtime as pallet_nomination_pools::Config>::StakeAdapter
					::member_delegation_balance(Member::from(
						member_account.clone(),
					))
					.unwrap_or_default();
				let trapped = actual.saturating_sub(expected);

				// Ignore dust amounts (< 1 DOT) — only claim meaningful trapped balances.
				if trapped >= DOT_DECIMALS {
					assert_ok!(NominationPools::<Runtime>::do_claim_trapped_balance(
						&member_account
					));

					success_count += 1;
					total_claimed += trapped;
					let whole = trapped / DOT_DECIMALS;
					let fraction = (trapped % DOT_DECIMALS) / (DOT_DECIMALS / 100);
					println!(
						"{:?},{},{}.{:02}",
						member_account, member_data.pool_id, whole, fraction
					);
				}
			}

			let total_whole = total_claimed / DOT_DECIMALS;
			let total_fraction = (total_claimed % DOT_DECIMALS) / (DOT_DECIMALS / 100);

			println!("\n--- Summary ---");
			println!("Total members: {}", total_members);
			println!("Successful claims: {}", success_count);
			println!("Total claimed: {}.{:02} DOT", total_whole, total_fraction);
		});
	}
}

#[test]
fn ah_treasury_creates_asset_reward_pool() {
	use frame_support::traits::schedule::DispatchTime;

	ExtBuilder::<Runtime>::default().build().execute_with(|| {
		let treasury_account: AccountId =
			asset_hub_westend_runtime::governance::TreasuryAccount::get();

		// Fund the treasury account so it exists and can hold the pool-creation deposit.
		assert_ok!(Balances::mint_into(&treasury_account, 100 * UNITS));

		let native = WestendLocation::get();
		let reward_rate_per_block = 1_000_000_000;

		assert_ok!(AssetRewards::create_pool(
			RuntimeOrigin::signed(treasury_account.clone()),
			Box::new(native.clone()),
			Box::new(native),
			reward_rate_per_block,
			DispatchTime::After(1_000_000),
			None,
		));

		assert_eq!(pallet_asset_rewards::Pools::<Runtime>::iter().count(), 1);
	});
}

mod dap {
	use super::*;

	#[test]
	fn tx_fees_go_to_dap_buffer() {
		let alice = AccountId::from(Sr25519Keyring::Alice);
		let buffer = <pallet_dap::Pallet<Runtime> as sp_staking::budget::BudgetRecipient<
			AccountId,
		>>::pot_account();
		let staging = pallet_dap::Pallet::<Runtime>::staging_account();
		let ed = ExistentialDeposit::get();

		ExtBuilder::<Runtime>::default()
			.with_collators(vec![alice.clone()])
			.with_session_keys(vec![(
				alice.clone(),
				alice.clone(),
				SessionKeys { aura: AuraId::from(Sr25519Keyring::Alice.public()) },
			)])
			.with_balances(vec![
				(alice.clone(), 100 * ed),
				(buffer.clone(), ed),
				(staging.clone(), ed),
			])
			.with_para_id(ASSET_HUB_ID.into())
			.build()
			.execute_with(|| {
				let alice_before = <Balances as Inspect<AccountId>>::balance(&alice);
				let buffer_before = <Balances as Inspect<AccountId>>::balance(&buffer);
				let staging_before = <Balances as Inspect<AccountId>>::balance(&staging);
				let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

				let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
				let xt = construct_extrinsic(Sr25519Keyring::Alice, call);
				assert_ok!(Executive::apply_extrinsic(xt).unwrap());

				let alice_after = <Balances as Inspect<AccountId>>::balance(&alice);
				let fee_paid = alice_before - alice_after;
				assert!(fee_paid > 0, "a fee should have been paid");

				// Fees land in staging first, not directly in the buffer.
				assert_eq!(
					<Balances as Inspect<AccountId>>::balance(&staging),
					staging_before + fee_paid
				);
				assert_eq!(<Balances as Inspect<AccountId>>::balance(&buffer), buffer_before);

				// on_idle drains staging into buffer and deactivates.
				pallet_dap::Pallet::<Runtime>::on_idle(1, Weight::MAX);

				assert_eq!(<Balances as Inspect<AccountId>>::balance(&staging), staging_before);
				assert_eq!(
					<Balances as Inspect<AccountId>>::balance(&buffer),
					buffer_before + fee_paid
				);
				assert_eq!(<Balances as Inspect<AccountId>>::total_issuance(), issuance_before);
			});
	}

	#[test]
	fn dust_removal_goes_to_dap_buffer() {
		let alice = AccountId::from(ALICE);
		let bob = AccountId::from(BOB);
		let buffer = <pallet_dap::Pallet<Runtime> as sp_staking::budget::BudgetRecipient<
			AccountId,
		>>::pot_account();
		let staging = pallet_dap::Pallet::<Runtime>::staging_account();
		let ed = ExistentialDeposit::get();
		let dust = ed / 2;

		ExtBuilder::<Runtime>::default()
			.with_collators(vec![AccountId::from(ALICE)])
			.with_session_keys(vec![(
				AccountId::from(ALICE),
				AccountId::from(ALICE),
				SessionKeys { aura: AuraId::from(sp_core::sr25519::Public::from_raw(ALICE)) },
			)])
			.build()
			.execute_with(|| {
				assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&bob, ed + dust));
				assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&alice, 100 * ed));
				assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&buffer, ed));
				// Pre-fund staging so dust (< ED) can be deposited without creating a new account.
				assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&staging, ed));

				let buffer_before = <Balances as Inspect<AccountId>>::balance(&buffer);
				let staging_before = <Balances as Inspect<AccountId>>::balance(&staging);
				let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

				// Transfer ED away from bob, leaving dust < ED → account reaped.
				assert_ok!(Balances::transfer_allow_death(
					RuntimeOrigin::signed(bob.clone()),
					alice.clone().into(),
					ed,
				));

				// Dust lands in staging first (two-phase deactivation).
				assert_eq!(
					<Balances as Inspect<AccountId>>::balance(&staging),
					staging_before + dust
				);
				assert_eq!(<Balances as Inspect<AccountId>>::balance(&buffer), buffer_before);
				assert_eq!(<Balances as Inspect<AccountId>>::balance(&bob), 0);

				// After on_idle: staging drains into buffer and deactivates.
				pallet_dap::Pallet::<Runtime>::on_idle(1, Weight::MAX);
				assert_eq!(<Balances as Inspect<AccountId>>::balance(&staging), staging_before);
				assert_eq!(
					<Balances as Inspect<AccountId>>::balance(&buffer),
					buffer_before + dust
				);
				assert_eq!(<Balances as Inspect<AccountId>>::total_issuance(), issuance_before);
			});
	}
}

// Exercises the real `ChargePGAS` extension pipeline via `Executive::apply_extrinsic`. The runtime
// overrides `CallFilter` to `Everything` under `runtime-benchmarks`, so these tests only make sense
// without that feature.
#[cfg(not(feature = "runtime-benchmarks"))]
mod pgas_allowance {
	use super::*;
	use asset_hub_westend_runtime::PGASAssetId;
	use sp_core::H160;
	use sp_runtime::BuildStorage;

	const SENDER: Sr25519Keyring = Sr25519Keyring::Bob;

	fn revive_call() -> RuntimeCall {
		RuntimeCall::Revive(pallet_revive::Call::call {
			dest: H160::default(),
			value: 0,
			weight_limit: Weight::zero(),
			storage_deposit_limit: 0,
			data: vec![],
		})
	}

	fn setup_ext(funded_accounts: Vec<(AccountId, Balance)>) -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		pallet_balances::GenesisConfig::<Runtime> {
			balances: funded_accounts,
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();
		pallet_assets::GenesisConfig::<Runtime, pallet_assets::Instance1> {
			assets: vec![(PGASAssetId::get(), AccountId::from(ALICE), true, 1)],
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();
		let mut ext: sp_io::TestExternalities = t.into();
		ext.execute_with(|| System::set_block_number(1));
		ext
	}

	fn mint_pgas(to: &AccountId, amount: Balance) {
		assert_ok!(<Assets as FungiblesMutate<_>>::mint_into(PGASAssetId::get(), to, amount));
	}

	fn pgas_balance(who: &AccountId) -> Balance {
		<Assets as FungiblesInspect<_>>::balance(PGASAssetId::get(), who)
	}

	fn pgas_fee_paid_event(who: &AccountId) -> Option<Balance> {
		System::events().into_iter().find_map(|e| match e.event {
			RuntimeEvent::PgasAllowance(pallet_pgas_allowance::Event::PGASFeePaid {
				who: w,
				actual_fee,
			}) if &w == who => Some(actual_fee),
			_ => None,
		})
	}

	/// Caller holds PGAS and dispatches a Revive call: fee is charged in PGAS and native is
	/// untouched.
	#[test]
	fn pgas_pays_for_revive_call() {
		let sender = SENDER.to_account_id();
		let initial_native = 10 * UNITS;
		let initial_pgas = 100 * UNITS;
		setup_ext(vec![(sender.clone(), initial_native)]).execute_with(|| {
			mint_pgas(&sender, initial_pgas);

			let native_before = <Balances as Inspect<_>>::balance(&sender);
			let pgas_before = pgas_balance(&sender);

			let xt = construct_extrinsic(SENDER, revive_call());
			assert_ok!(Executive::apply_extrinsic(xt).unwrap());

			let native_after = <Balances as Inspect<_>>::balance(&sender);
			let pgas_after = pgas_balance(&sender);

			assert_eq!(native_before, native_after, "native untouched on PGAS path");
			let fee = pgas_before.checked_sub(pgas_after).expect("PGAS charged");
			assert!(fee > 0);
			assert_eq!(pgas_fee_paid_event(&sender), Some(fee));
		});
	}

	/// Caller holds no PGAS: the extension falls through to the inner tx-payment and native is
	/// charged; no `PGASFeePaid` event is emitted.
	#[test]
	fn falls_back_to_native_when_caller_has_no_pgas() {
		let sender = SENDER.to_account_id();
		let initial_native = 10 * UNITS;
		setup_ext(vec![(sender.clone(), initial_native)]).execute_with(|| {
			let native_before = <Balances as Inspect<_>>::balance(&sender);

			let xt = construct_extrinsic(SENDER, revive_call());
			assert_ok!(Executive::apply_extrinsic(xt).unwrap());

			let native_after = <Balances as Inspect<_>>::balance(&sender);
			assert!(native_after < native_before, "native should have been charged");
			assert_eq!(pgas_balance(&sender), 0);
			assert_eq!(pgas_fee_paid_event(&sender), None);
		});
	}

	/// Caller holds PGAS but dispatches a non-Revive call: the filter misses, PGAS is not
	/// touched, and native pays the fee.
	#[test]
	fn filter_miss_uses_native_even_with_pgas() {
		let sender = SENDER.to_account_id();
		let initial_native = 10 * UNITS;
		let initial_pgas = 100 * UNITS;
		setup_ext(vec![(sender.clone(), initial_native)]).execute_with(|| {
			mint_pgas(&sender, initial_pgas);

			let pgas_before = pgas_balance(&sender);
			let native_before = <Balances as Inspect<_>>::balance(&sender);

			let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
			let xt = construct_extrinsic(SENDER, call);
			assert_ok!(Executive::apply_extrinsic(xt).unwrap());

			assert_eq!(pgas_balance(&sender), pgas_before, "PGAS untouched on filter miss");
			let native_after = <Balances as Inspect<_>>::balance(&sender);
			assert!(native_after < native_before, "native charged");
			assert_eq!(pgas_fee_paid_event(&sender), None);
		});
	}
}

// Regression tests for the revive trace-replay proof-size reclaim fix: replaying a block via
// `trace_block`/`trace_tx` registers a proof recorder so the accumulated worst-case `proof_size` is
// reclaimed instead of tripping `ExhaustsResources` and dropping the tail's traces.
mod revive_trace_reclaim {
	use super::*;
	use frame_support::dispatch::DispatchClass;
	use frame_system::pallet_prelude::HeaderFor;
	use pallet_revive::{
		pallet_revive_types::runtime_api::{
			TraceBlockInputPayloadV1, TraceBlockVersionedInputPayload,
			TraceBlockVersionedOutputPayload, TraceTxInputPayloadV1, TraceTxVersionedInputPayload,
			TraceTxVersionedOutputPayload, TraceV1, TracerTypeV1,
		},
		runtime_decl_for_revive_api::ReviveApiV2,
	};
	use pallet_revive_fixtures::compile_module;
	use sp_core::H160;
	use sp_runtime::{traits::Header as _, BuildStorage};
	use sp_trie::{proof_size_extension::ProofSizeExt, ProofSizeProvider};

	const SENDER: Sr25519Keyring = Sr25519Keyring::Bob;
	// Enough reads that each call meters ~the per-call proof_size limit.
	const ROUNDS: u32 = 100_000;

	// Reports a constant size, so the per-extrinsic proof diff is zero: models a recorder being
	// present, letting reclaim refund the full over-charge.
	struct ConstantRecorder;
	impl ProofSizeProvider for ConstantRecorder {
		fn estimate_encoded_size(&self) -> usize {
			0
		}
	}

	fn setup_ext() -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		pallet_balances::GenesisConfig::<Runtime> {
			balances: vec![
				(SENDER.to_account_id(), 1_000_000_000 * UNITS),
				(pallet_revive::Pallet::<Runtime>::account_id(), 1_000_000 * UNITS),
			],
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();
		let mut ext: sp_io::TestExternalities = t.into();
		ext.execute_with(|| System::set_block_number(1));
		ext
	}

	fn signed_revive_call(addr: H160, nonce: u32, weight_limit: Weight) -> UncheckedExtrinsic {
		let call = RuntimeCall::Revive(pallet_revive::Call::call {
			dest: addr,
			value: 0,
			weight_limit,
			storage_deposit_limit: 0,
			data: ROUNDS.to_le_bytes().to_vec(),
		});
		construct_extrinsic_with_nonce(SENDER, call, nonce)
	}

	// Deploy the repeated-read contract, build a block of two calls, and replay it through `f`
	// (`trace_block` or `trace_tx`), registering the proof recorder when `with_recorder`.
	fn with_block<R>(with_recorder: bool, f: impl FnOnce(Block) -> R) -> R {
		let code = compile_module("repeated_storage_read").unwrap().0;
		let mut ext = setup_ext();
		if with_recorder {
			ext.register_extension(ProofSizeExt::new(ConstantRecorder));
		}
		ext.execute_with(|| {
			let budget = <Runtime as frame_system::Config>::BlockWeights::get()
				.get(DispatchClass::Normal)
				.max_total
				.expect("normal class has a max_total; qed")
				.proof_size();
			// ~60% of the budget each, so the two calls only both fit when reclaim is in effect.
			let weight_limit = Weight::from_parts(500_000_000_000, budget * 3 / 5);

			let contract = bare_instantiate(&SENDER.to_account_id(), code)
				.transaction_limits(TransactionLimits::WeightAndDeposit {
					weight_limit: Weight::from_parts(500_000_000_000, 10 * 1024 * 1024),
					deposit_limit: Balance::MAX,
				})
				.build_and_unwrap_contract();

			// deploying bumped the sender's nonce
			let base = frame_system::Pallet::<Runtime>::account(&SENDER.to_account_id()).nonce;
			let extrinsics = vec![
				signed_revive_call(contract.addr, base, weight_limit),
				signed_revive_call(contract.addr, base + 1, weight_limit),
			];
			let header = <HeaderFor<Runtime>>::new(
				frame_system::Pallet::<Runtime>::block_number() + 1,
				Default::default(),
				Default::default(),
				Default::default(),
				Default::default(),
			);

			f(Block { header, extrinsics })
		})
	}

	fn tracer() -> TracerTypeV1 {
		TracerTypeV1::CallTracer(None)
	}

	fn trace_block(block: Block) -> usize {
		let input = TraceBlockVersionedInputPayload::V1(TraceBlockInputPayloadV1 {
			block,
			config: tracer(),
		});
		let TraceBlockVersionedOutputPayload::V1(output) = Runtime::trace_block_versioned(input)
		else {
			panic!("v1 input must produce v1 output");
		};
		output.traces.len()
	}

	fn trace_tx(block: Block, tx_index: u32) -> Option<TraceV1> {
		let input = TraceTxVersionedInputPayload::V1(TraceTxInputPayloadV1 {
			block,
			tx_index,
			config: tracer(),
		});
		let TraceTxVersionedOutputPayload::V1(output) = Runtime::trace_tx_versioned(input) else {
			panic!("v1 input must produce v1 output");
		};
		output.trace
	}

	#[test]
	fn trace_block_drops_tail_trace_without_proof_recorder() {
		let with_recorder = with_block(true, trace_block);
		let without = with_block(false, trace_block);
		assert_eq!(with_recorder, 2, "both calls traced with a recorder");
		assert!(without < with_recorder, "tail trace dropped without a recorder");
	}

	#[test]
	fn trace_tx_drops_tail_trace_without_proof_recorder() {
		let with_recorder = with_block(true, |b| trace_tx(b, 1));
		let without = with_block(false, |b| trace_tx(b, 1));
		assert!(with_recorder.is_some(), "tail tx traced with a recorder");
		assert!(without.is_none(), "tail tx trace dropped without a recorder");
	}
}
