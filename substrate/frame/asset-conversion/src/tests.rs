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

use crate::{mock::*, *};
use frame_support::{
	assert_noop, assert_ok, assert_storage_noop,
	instances::Instance1,
	traits::{
		fungible,
		fungible::{Inspect as FungibleInspect, NativeOrWithId},
		fungibles,
		fungibles::{Inspect, InspectEnumerable},
		Get, LockableCurrency, WithdrawReasons,
	},
};
use sp_arithmetic::Permill;
use sp_runtime::{BuildStorage, DispatchError, TokenError};

fn events() -> Vec<Event<Test>> {
	let result = System::events()
		.into_iter()
		.map(|r| r.event)
		.filter_map(|e| {
			if let mock::RuntimeEvent::AssetConversion(inner) = e {
				Some(inner)
			} else {
				None
			}
		})
		.collect();

	System::reset_events();

	result
}

fn pools() -> Vec<<Test as Config>::PoolId> {
	let mut s: Vec<_> = Pools::<Test>::iter().map(|x| x.0).collect();
	s.sort();
	s
}

fn assets() -> Vec<NativeOrWithId<u32>> {
	let mut s: Vec<_> = Assets::asset_ids().map(|id| NativeOrWithId::WithId(id)).collect();
	s.sort();
	s
}

fn pool_assets() -> Vec<u32> {
	let mut s: Vec<_> = <<Test as Config>::PoolAssets>::asset_ids().collect();
	s.sort();
	s
}

fn create_tokens(owner: u128, tokens: Vec<NativeOrWithId<u32>>) {
	create_tokens_with_ed(owner, tokens, 1)
}

fn create_tokens_with_ed(owner: u128, tokens: Vec<NativeOrWithId<u32>>, ed: u128) {
	for token_id in tokens {
		let asset_id = match token_id {
			NativeOrWithId::WithId(id) => id,
			_ => unreachable!("invalid token"),
		};
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), asset_id, owner, false, ed));
	}
}

fn balance(owner: u128, token_id: NativeOrWithId<u32>) -> u128 {
	<<Test as Config>::Assets>::balance(token_id, &owner)
}

fn pool_balance(owner: u128, token_id: u32) -> u128 {
	<<Test as Config>::PoolAssets>::balance(token_id, owner)
}

fn get_native_ed() -> u128 {
	<<Test as Config>::Assets>::minimum_balance(NativeOrWithId::Native)
}

macro_rules! bvec {
	($($x:expr),+ $(,)?) => (
		vec![$( Box::new( $x ), )*]
	)
}

#[test]
fn validate_with_first_asset_pool_id_locator() {
	new_test_ext().execute_with(|| {
		use NativeOrWithId::{Native, WithId};
		assert_eq!(WithFirstAssetLocator::pool_id(&Native, &WithId(2)), Ok((Native, WithId(2))));
		assert_eq!(WithFirstAssetLocator::pool_id(&WithId(2), &Native), Ok((Native, WithId(2))));
		assert_noop!(WithFirstAssetLocator::pool_id(&Native, &Native), ());
		assert_noop!(WithFirstAssetLocator::pool_id(&WithId(2), &WithId(1)), ());
	});
}

#[test]
fn validate_ascending_pool_id_locator() {
	new_test_ext().execute_with(|| {
		use NativeOrWithId::{Native, WithId};
		assert_eq!(AscendingLocator::pool_id(&Native, &WithId(2)), Ok((Native, WithId(2))));
		assert_eq!(AscendingLocator::pool_id(&WithId(2), &Native), Ok((Native, WithId(2))));
		assert_eq!(AscendingLocator::pool_id(&WithId(2), &WithId(1)), Ok((WithId(1), WithId(2))));
		assert_eq!(AscendingLocator::pool_id(&Native, &Native), Err(()));
		assert_eq!(AscendingLocator::pool_id(&WithId(1), &WithId(1)), Err(()));
	});
}

#[test]
fn validate_native_or_with_id_sorting() {
	new_test_ext().execute_with(|| {
		use NativeOrWithId::{Native, WithId};
		assert!(WithId(2) > WithId(1));
		assert!(WithId(1) <= WithId(1));
		assert_eq!(WithId(1), WithId(1));
		assert_eq!(Native::<u32>, Native::<u32>);
		assert!(Native < WithId(1));
	});
}

#[test]
fn check_pool_accounts_dont_collide() {
	use std::collections::HashSet;
	let mut map = HashSet::new();

	for i in 0..1_000_000u32 {
		let account: u128 = <Test as Config>::PoolLocator::address(&(
			NativeOrWithId::Native,
			NativeOrWithId::WithId(i),
		))
		.unwrap();
		if map.contains(&account) {
			panic!("Collision at {}", i);
		}
		map.insert(account);
	}
}

#[test]
fn check_max_numbers() {
	new_test_ext().execute_with(|| {
		assert_eq!(AssetConversion::quote(&3u128, &u128::MAX, &u128::MAX).ok().unwrap(), 3);
		assert!(AssetConversion::quote(&u128::MAX, &3u128, &u128::MAX).is_err());
		assert_eq!(AssetConversion::quote(&u128::MAX, &u128::MAX, &1u128).ok().unwrap(), 1);

		assert_eq!(
			AssetConversion::get_amount_out(LpFee::get(), &100u128, &u128::MAX, &u128::MAX)
				.ok()
				.unwrap(),
			99
		);
		assert_eq!(
			AssetConversion::get_amount_in(LpFee::get(), &100u128, &u128::MAX, &u128::MAX)
				.ok()
				.unwrap(),
			101
		);
	});
}

#[test]
fn can_create_pool() {
	new_test_ext().execute_with(|| {
		let asset_account_deposit: u128 =
			<mock::Test as pallet_assets::Config<Instance1>>::AssetAccountDeposit::get();
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());

		create_tokens(user, vec![token_2.clone()]);

		let lp_token = AssetConversion::get_next_pool_asset_id();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 1000));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_1.clone())
		));

		let setup_fee = <<Test as Config>::PoolSetupFee as Get<<Test as Config>::Balance>>::get();
		let pool_account = AssetConversionOrigin::get();
		assert_eq!(
			balance(user, NativeOrWithId::Native),
			1000 - (setup_fee + asset_account_deposit)
		);
		assert_eq!(balance(pool_account, NativeOrWithId::Native), setup_fee);
		assert_eq!(lp_token + 1, AssetConversion::get_next_pool_asset_id());

		assert_eq!(
			events(),
			[Event::<Test>::PoolCreated {
				creator: user,
				pool_id: pool_id.clone(),
				pool_account: <Test as Config>::PoolLocator::address(&pool_id).unwrap(),
				lp_token
			}]
		);
		assert_eq!(pools(), vec![pool_id]);
		assert_eq!(assets(), vec![token_2.clone()]);
		assert_eq!(pool_assets(), vec![lp_token]);

		assert_noop!(
			AssetConversion::create_pool(
				RuntimeOrigin::signed(user),
				Box::new(token_1.clone()),
				Box::new(token_1.clone())
			),
			Error::<Test>::InvalidAssetPair
		);
		assert_noop!(
			AssetConversion::create_pool(
				RuntimeOrigin::signed(user),
				Box::new(token_2.clone()),
				Box::new(token_2.clone())
			),
			Error::<Test>::InvalidAssetPair
		);

		// validate we cannot create WithId(1)/WithId(2) pool
		let token_1 = NativeOrWithId::WithId(1);
		create_tokens(user, vec![token_1.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
	});
}

#[test]
fn create_same_pool_twice_should_fail() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);

		let lp_token = AssetConversion::get_next_pool_asset_id();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_1.clone())
		));
		let expected_free = lp_token + 1;
		assert_eq!(expected_free, AssetConversion::get_next_pool_asset_id());

		assert_noop!(
			AssetConversion::create_pool(
				RuntimeOrigin::signed(user),
				Box::new(token_2.clone()),
				Box::new(token_1.clone())
			),
			Error::<Test>::PoolExists
		);
		assert_eq!(expected_free, AssetConversion::get_next_pool_asset_id());

		// Try switching the same tokens around:
		assert_noop!(
			AssetConversion::create_pool(
				RuntimeOrigin::signed(user),
				Box::new(token_1.clone()),
				Box::new(token_2.clone())
			),
			Error::<Test>::PoolExists
		);
		assert_eq!(expected_free, AssetConversion::get_next_pool_asset_id());
	});
}

#[test]
fn different_pools_should_have_different_lp_tokens() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);
		let pool_id_1_2 = (token_1.clone(), token_2.clone());
		let pool_id_1_3 = (token_1.clone(), token_3.clone());

		create_tokens(user, vec![token_2.clone(), token_3.clone()]);

		let lp_token2_1 = AssetConversion::get_next_pool_asset_id();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_1.clone())
		));
		let lp_token3_1 = AssetConversion::get_next_pool_asset_id();

		assert_eq!(
			events(),
			[Event::<Test>::PoolCreated {
				creator: user,
				pool_id: pool_id_1_2.clone(),
				pool_account: <Test as Config>::PoolLocator::address(&pool_id_1_2).unwrap(),
				lp_token: lp_token2_1
			}]
		);

		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_3.clone()),
			Box::new(token_1.clone())
		));
		assert_eq!(
			events(),
			[Event::<Test>::PoolCreated {
				creator: user,
				pool_id: pool_id_1_3.clone(),
				pool_account: <Test as Config>::PoolLocator::address(&pool_id_1_3).unwrap(),
				lp_token: lp_token3_1,
			}]
		);

		assert_ne!(lp_token2_1, lp_token3_1);
	});
}

#[test]
fn can_add_liquidity() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);

		create_tokens(user, vec![token_2.clone(), token_3.clone()]);
		let lp_token1 = AssetConversion::get_next_pool_asset_id();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		let lp_token2 = AssetConversion::get_next_pool_asset_id();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_3.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10000 * 2 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			10,
			10000,
			10,
			user,
		));

		let pool_id = (token_1.clone(), token_2.clone());
		assert!(events().contains(&Event::<Test>::LiquidityAdded {
			who: user,
			mint_to: user,
			pool_id: pool_id.clone(),
			amount1_provided: 10000,
			amount2_provided: 10,
			lp_token: lp_token1,
			lp_token_minted: 216,
		}));
		let pallet_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		assert_eq!(balance(pallet_account, token_1.clone()), 10000);
		assert_eq!(balance(pallet_account, token_2.clone()), 10);
		assert_eq!(balance(user, token_1.clone()), 10000 + ed);
		assert_eq!(balance(user, token_2.clone()), 1000 - 10);
		assert_eq!(pool_balance(user, lp_token1), 216);

		// try to pass the non-native - native assets, the result should be the same
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_3.clone()),
			Box::new(token_1.clone()),
			10,
			10000,
			10,
			10000,
			user,
		));

		let pool_id = (token_1.clone(), token_3.clone());
		assert!(events().contains(&Event::<Test>::LiquidityAdded {
			who: user,
			mint_to: user,
			pool_id: pool_id.clone(),
			amount1_provided: 10,
			amount2_provided: 10000,
			lp_token: lp_token2,
			lp_token_minted: 216,
		}));
		let pallet_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		assert_eq!(balance(pallet_account, token_1.clone()), 10000);
		assert_eq!(balance(pallet_account, token_3.clone()), 10);
		assert_eq!(balance(user, token_1.clone()), ed);
		assert_eq!(balance(user, token_3.clone()), 1000 - 10);
		assert_eq!(pool_balance(user, lp_token2), 216);
	});
}

#[test]
fn add_tiny_liquidity_leads_to_insufficient_liquidity_minted_error() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 1000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_noop!(
			AssetConversion::add_liquidity(
				RuntimeOrigin::signed(user),
				Box::new(token_1.clone()),
				Box::new(token_2.clone()),
				1,
				1,
				1,
				1,
				user
			),
			Error::<Test>::AmountOneLessThanMinimal
		);

		assert_noop!(
			AssetConversion::add_liquidity(
				RuntimeOrigin::signed(user),
				Box::new(token_1.clone()),
				Box::new(token_2.clone()),
				get_native_ed(),
				1,
				1,
				1,
				user
			),
			Error::<Test>::InsufficientLiquidityMinted
		);
	});
}

#[test]
fn add_tiny_liquidity_directly_to_pool_address() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);

		create_tokens(user, vec![token_2.clone(), token_3.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_3.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10000 * 2 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, 1000));

		// check we're still able to add the liquidity even when the pool already has some
		// token_1.clone()
		let pallet_account =
			<Test as Config>::PoolLocator::address(&(token_1.clone(), token_2.clone())).unwrap();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), pallet_account, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			10,
			10000,
			10,
			user,
		));

		// check the same but for token_3.clone() (non-native token)
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_3.clone()),
			10000,
			10,
			10000,
			10,
			user,
		));
	});
}

#[test]
fn can_remove_liquidity() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());

		create_tokens(user, vec![token_2.clone()]);
		let lp_token = AssetConversion::get_next_pool_asset_id();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed_token_1 = <Balances as fungible::Inspect<_>>::minimum_balance();
		let ed_token_2 = <Assets as fungibles::Inspect<_>>::minimum_balance(2);
		assert_ok!(Balances::force_set_balance(
			RuntimeOrigin::root(),
			user,
			10000000000 + ed_token_1
		));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 100000 + ed_token_2));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			1000000000,
			100000,
			1000000000,
			100000,
			user,
		));

		let total_lp_received = pool_balance(user, lp_token);
		LiquidityWithdrawalFee::set(&Permill::from_percent(10));

		assert_ok!(AssetConversion::remove_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			total_lp_received,
			0,
			0,
			user,
		));

		assert!(events().contains(&Event::<Test>::LiquidityRemoved {
			who: user,
			withdraw_to: user,
			pool_id: pool_id.clone(),
			amount1: 899991000,
			amount2: 89999,
			lp_token,
			lp_token_burned: total_lp_received,
			withdrawal_fee: <Test as Config>::LiquidityWithdrawalFee::get()
		}));

		let pool_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		assert_eq!(balance(pool_account, token_1.clone()), 100009000);
		assert_eq!(balance(pool_account, token_2.clone()), 10001);
		assert_eq!(pool_balance(pool_account, lp_token), 100);

		assert_eq!(
			balance(user, token_1.clone()),
			10000000000 - 1000000000 + 899991000 + ed_token_1
		);
		assert_eq!(balance(user, token_2.clone()), 89999 + ed_token_2);
		assert_eq!(pool_balance(user, lp_token), 0);
	});
}

#[test]
fn can_not_redeem_more_lp_tokens_than_were_minted() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let lp_token = AssetConversion::get_next_pool_asset_id();

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(
			RuntimeOrigin::root(),
			user,
			10000 + get_native_ed()
		));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			10,
			10000,
			10,
			user,
		));

		// Only 216 lp_tokens_minted
		assert_eq!(pool_balance(user, lp_token), 216);

		assert_noop!(
			AssetConversion::remove_liquidity(
				RuntimeOrigin::signed(user),
				Box::new(token_1.clone()),
				Box::new(token_2.clone()),
				216 + 1, // Try and redeem 10 lp tokens while only 9 minted.
				0,
				0,
				user,
			),
			DispatchError::Token(TokenError::FundsUnavailable)
		);
	});
}

#[test]
fn quote_price_returns_none_for_zero_amount() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 100000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			200,
			1,
			1,
			user,
		));

		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_1.clone(),
				token_2.clone(),
				0,
				true,
			),
			None
		);
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				token_1.clone(),
				token_2.clone(),
				0,
				true,
			),
			None
		);
	});
}

#[test]
fn quote_price_returns_none_for_zero_output() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10_000_000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		// Create a heavily skewed pool: lots of asset1, very little asset2.
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			1_000_000,
			200,
			1,
			1,
			user,
		));

		// Tiny input into a skewed pool rounds output to zero.
		// get_amount_out(1, 1_000_000, 200) = 1*997*200 / (1_000_000*1000 + 997) = 0
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_1.clone(),
				token_2.clone(),
				1,
				true,
			),
			None
		);
		// Without fees: quote(1, 1_000_000, 200) = 1*200/1_000_000 = 0
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_1.clone(),
				token_2.clone(),
				1,
				false,
			),
			None
		);
	});
}

#[test]
fn can_quote_price() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 100000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			200,
			1,
			1,
			user,
		));

		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				3000,
				false,
			),
			Some(60)
		);
		// including fee so should get less out...
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				3000,
				true,
			),
			Some(46)
		);
		// Check it still gives same price:
		// (if the above accidentally exchanged then it would not give same quote as before)
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				3000,
				false,
			),
			Some(60)
		);
		// including fee so should get less out...
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				3000,
				true,
			),
			Some(46)
		);

		// Check inverse:
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				60,
				false,
			),
			Some(3000)
		);
		// including fee so should get less out...
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				60,
				true,
			),
			Some(2302)
		);

		// same tests as above but for quote_price_tokens_for_exact_tokens:
		//
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				60,
				false,
			),
			Some(3000)
		);
		// including fee so should need to put more in...
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				60,
				true,
			),
			Some(4299)
		);
		// Check it still gives same price:
		// (if the above accidentally exchanged then it would not give same quote as before)
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				60,
				false,
			),
			Some(3000)
		);
		// including fee so should need to put more in...
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				60,
				true,
			),
			Some(4299)
		);

		// Check inverse:
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				3000,
				false,
			),
			Some(60)
		);
		// including fee so should need to put more in...
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				3000,
				true,
			),
			Some(86)
		);

		// roundtrip: Without fees one should get the original number
		//
		let amount_in = 100;

		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				amount_in,
				false,
			)
			.and_then(|amount| AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				amount,
				false,
			)),
			Some(amount_in)
		);
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				amount_in,
				false,
			)
			.and_then(|amount| AssetConversion::quote_price_exact_tokens_for_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				amount,
				false,
			)),
			Some(amount_in)
		);

		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				amount_in,
				false,
			)
			.and_then(|amount| AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				amount,
				false,
			)),
			Some(amount_in)
		);
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::Native,
				NativeOrWithId::WithId(2),
				amount_in,
				false,
			)
			.and_then(|amount| AssetConversion::quote_price_tokens_for_exact_tokens(
				NativeOrWithId::WithId(2),
				NativeOrWithId::Native,
				amount,
				false,
			)),
			Some(amount_in)
		);
	});
}

#[test]
fn quote_price_ignores_unrelated_non_sufficient_assets_in_pool_account() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let dot = NativeOrWithId::Native;
		let token = NativeOrWithId::WithId(2);
		let pool_id = (dot.clone(), token.clone());

		create_tokens(user, vec![token.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(dot.clone()),
			Box::new(token.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 100000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(dot.clone()),
			Box::new(token.clone()),
			10000,
			200,
			1,
			1,
			user,
		));

		let price = AssetConversion::quote_price_exact_tokens_for_tokens(
			token.clone(),
			dot.clone(),
			60,
			true,
		);
		assert_eq!(price, Some(2302));

		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 3, user, false, 1));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, 1000));
		let pool_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(user), 3, pool_account, 1));

		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(token, dot, 60, true),
			price
		);
	});
}

#[test]
fn quote_price_exact_tokens_for_tokens_matches_execution() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 100000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			200,
			1,
			1,
			user,
		));

		let amount = 1;
		let quoted_price = 49;
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_2.clone(),
				token_1.clone(),
				amount,
				true,
			),
			Some(quoted_price)
		);

		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user2, amount));
		let prior_dot_balance = 20000;
		assert_eq!(prior_dot_balance, balance(user2, token_1.clone()));
		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user2),
			bvec![token_2.clone(), token_1.clone()],
			amount,
			1,
			user2,
			false,
		));

		assert_eq!(prior_dot_balance + quoted_price, balance(user2, token_1.clone()));
	});
}

#[test]
fn quote_price_tokens_for_exact_tokens_matches_execution() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 100000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			200,
			1,
			1,
			user,
		));

		let amount = 49;
		let quoted_price = 1;
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				token_2.clone(),
				token_1.clone(),
				amount,
				true,
			),
			Some(quoted_price)
		);

		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user2, amount));
		let prior_dot_balance = 20000;
		assert_eq!(prior_dot_balance, balance(user2, token_1.clone()));
		let prior_asset_balance = 49;
		assert_eq!(prior_asset_balance, balance(user2, token_2.clone()));
		assert_ok!(AssetConversion::swap_tokens_for_exact_tokens(
			RuntimeOrigin::signed(user2),
			bvec![token_2.clone(), token_1.clone()],
			amount,
			1,
			user2,
			false,
		));

		assert_eq!(prior_dot_balance + amount, balance(user2, token_1.clone()));
		assert_eq!(prior_asset_balance - quoted_price, balance(user2, token_2.clone()));
	});
}

#[test]
fn quote_price_returns_none_when_output_exceeds_pool_withdrawable() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let ed = 1000;

		create_tokens_with_ed(user, vec![token_2.clone()], ed);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10_000_000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 10_000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			1_000_000,
			2_000,
			1,
			1,
			user,
		));
		// Pool has 1_000_000 native and 2_000 of asset2 (min_balance=1000).
		// Pool can only send out up to 2_000 - 1_000 = 1_000 of asset2 while staying alive.

		// quote_price_tokens_for_exact_tokens: requesting exact output.
		// Requesting 1001 of asset2 exceeds available (1000), must return None.
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				token_1.clone(),
				token_2.clone(),
				1001,
				true,
			),
			None
		);
		// Also without fees.
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				token_1.clone(),
				token_2.clone(),
				1001,
				false,
			),
			None
		);
		// Requesting exactly 1000 should succeed (at the boundary).
		assert!(AssetConversion::quote_price_tokens_for_exact_tokens(
			token_1.clone(),
			token_2.clone(),
			1000,
			true,
		)
		.is_some());

		// quote_price_exact_tokens_for_tokens: given input, computed output must fit.
		// With reserves (1_000_000, 2_000), fee=0.3%, and max_output=1000:
		// input=1_005_018, output=1001 > 1000, must return None.
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_1.clone(),
				token_2.clone(),
				1_005_018,
				true,
			),
			None
		);
		// Also without fees.
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_1.clone(),
				token_2.clone(),
				1_005_018,
				false,
			),
			None
		);
		// input=1_005_017, output=1000 ≤ 1000, must return Some.
		assert!(AssetConversion::quote_price_exact_tokens_for_tokens(
			token_1.clone(),
			token_2.clone(),
			1_005_017,
			true,
		)
		.is_some());
	});
}

#[test]
fn blocked_or_frozen_pool_is_unavailable() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		// Set up a native/token_2 pool. `user` creates token_2, which makes it the token's
		// freezer and lets it block and freeze the pool account later.
		create_tokens_with_ed(user, vec![token_2.clone()], 1);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10_000_000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 10_000));
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			1_000_000,
			2_000,
			1,
			1,
			user,
		));

		let pool_id = (token_1.clone(), token_2.clone());
		let pool_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();

		// Quotes for selling token_2 to the pool, using both the exact input and the exact
		// output quote functions.
		let quote_token_2_to_native = || {
			[
				AssetConversion::quote_price_exact_tokens_for_tokens(
					token_2.clone(),
					token_1.clone(),
					100,
					true,
				),
				AssetConversion::quote_price_tokens_for_exact_tokens(
					token_2.clone(),
					token_1.clone(),
					100,
					true,
				),
			]
		};

		// Normal pool: both quotes return a price.
		assert!(quote_token_2_to_native().iter().all(Option::is_some));

		// Blocked pool can't send token_2, so no quote.
		assert_ok!(Assets::block(RuntimeOrigin::signed(user), 2, pool_account));
		assert_eq!(quote_token_2_to_native(), [None, None]);

		// Unblocked: quotes work again.
		assert_ok!(Assets::thaw(RuntimeOrigin::signed(user), 2, pool_account));
		assert!(quote_token_2_to_native().iter().all(Option::is_some));

		// Frozen asset class: no quote, until it's thawed.
		assert_ok!(Assets::freeze_asset(RuntimeOrigin::signed(user), 2));
		assert_eq!(quote_token_2_to_native(), [None, None]);
		assert_ok!(Assets::thaw_asset(RuntimeOrigin::signed(user), 2));
		assert!(quote_token_2_to_native().iter().all(Option::is_some));

		// Frozen pool can't send token_2, so it's halted in both directions.
		assert_ok!(Assets::freeze(RuntimeOrigin::signed(user), 2, pool_account));
		assert_eq!(quote_token_2_to_native(), [None, None]);
		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				100,
				1,
				user,
				false,
			),
			Error::<Test>::PoolUnavailable
		);
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_1.clone(),
				token_2.clone(),
				100,
				true,
			),
			None
		);
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				token_1.clone(),
				token_2.clone(),
				100,
				true,
			),
			None
		);
	});
}

#[test]
fn pool_with_native_reserve_at_minimum_can_still_be_refilled() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let native = NativeOrWithId::Native;
		let token = NativeOrWithId::WithId(2);
		let native_ed = <Test as pallet_balances::Config>::ExistentialDeposit::get();
		let native_liquidity = 1_000_000;

		// Create a native/token pool.
		create_tokens_with_ed(user, vec![token.clone()], 1);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(native.clone()),
			Box::new(token.clone())
		));
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10_000_000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1_000_000_000));
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(native.clone()),
			Box::new(token.clone()),
			native_liquidity,
			2_000,
			1,
			1,
			user,
		));
		let pool_id = (native.clone(), token.clone());
		let pool_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();

		// Send the pool account an unrelated non-sufficient asset. This is what makes its
		// native ED unspendable, as on live chains.
		assert_ok!(Assets::force_create(RuntimeOrigin::root(), 3, user, false, 1));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, 1000));
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(user), 3, pool_account, 1));

		// Buy out all native the pool can give, so it holds exactly the ED.
		assert_ok!(AssetConversion::swap_tokens_for_exact_tokens(
			RuntimeOrigin::signed(user),
			bvec![token.clone(), native.clone()],
			native_liquidity - native_ed,
			u128::MAX,
			user,
			false,
		));
		assert_eq!(Balances::free_balance(pool_account), native_ed);

		// The pool is at its minimum but not frozen, so selling native into it still works.
		assert!(AssetConversion::quote_price_exact_tokens_for_tokens(
			native.clone(),
			token.clone(),
			1_000,
			true,
		)
		.is_some());
		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![native.clone(), token.clone()],
			1_000,
			1,
			user,
			false,
		));
		assert_eq!(Balances::free_balance(pool_account), native_ed + 1_000);
	});
}

#[test]
fn pool_with_locked_native_is_still_usable() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let native = NativeOrWithId::Native;
		let token = NativeOrWithId::WithId(2);

		create_tokens_with_ed(user, vec![token.clone()], 1);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(native.clone()),
			Box::new(token.clone())
		));
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10_000_000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 10_000));
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(native.clone()),
			Box::new(token.clone()),
			1_000_000,
			2_000,
			1,
			1,
			user,
		));
		let pool_id = (native.clone(), token.clone());
		let pool_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();

		let (native_reserve, token_reserve) =
			AssetConversion::get_reserves(native.clone(), token.clone()).unwrap();

		// Anyone can send the pool funds locked under a vesting schedule.
		assert_ok!(Balances::transfer_allow_death(
			RuntimeOrigin::signed(user),
			pool_account,
			10_000
		));
		Balances::set_lock(*b"vesting ", &pool_account, 10_000, WithdrawReasons::all());

		// The pool's native reserve grows by the vested amount and nothing else changes.
		assert_eq!(
			AssetConversion::get_reserves(native.clone(), token.clone()).unwrap(),
			(native_reserve + 10_000, token_reserve),
		);

		// The pool still works in both directions.
		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![token.clone(), native.clone()],
			100,
			1,
			user,
			false,
		));
		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![native.clone(), token.clone()],
			10_000,
			1,
			user,
			false,
		));
	});
}

#[test]
fn quote_price_returns_none_swap_fails() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let ed = 1000;

		create_tokens_with_ed(user, vec![token_2.clone()], ed);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10_000_000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 10_000));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			1_000_000,
			2_000,
			1,
			1,
			user,
		));

		// The AMM formula can compute a price for 1001 output (it's below reserve of 2000),
		// but the pool can only deliver 1000 (2000 - 1000 min_balance).
		// Verify the raw AMM math would produce a result...
		let (reserve_in, reserve_out) =
			AssetConversion::get_reserves(token_1.clone(), token_2.clone()).unwrap();
		assert!(
			Pallet::<Test>::get_amount_in(LpFee::get(), &1001, &reserve_in, &reserve_out).is_ok()
		);
		// ...but the quote correctly returns None.
		assert_eq!(
			AssetConversion::quote_price_tokens_for_exact_tokens(
				token_1.clone(),
				token_2.clone(),
				1001,
				true,
			),
			None
		);

		// And verify the actual swap would indeed fail.
		let swapper = 3;
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), swapper, 10_000_000));
		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(swapper),
				bvec![token_1.clone(), token_2.clone()],
				1001,
				10_000_000,
				swapper,
				false,
			),
			TokenError::NotExpendable
		);
	});
}

#[test]
fn can_swap_with_native() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let input_amount = 100;
		let expect_receive =
			AssetConversion::get_amount_out(LpFee::get(), &input_amount, &liquidity2, &liquidity1)
				.ok()
				.unwrap();

		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_2.clone(), token_1.clone()],
			input_amount,
			1,
			user,
			false,
		));

		let pallet_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		assert_eq!(balance(user, token_1.clone()), expect_receive + ed);
		assert_eq!(balance(user, token_2.clone()), 1000 - liquidity2 - input_amount);
		assert_eq!(balance(pallet_account, token_1.clone()), liquidity1 - expect_receive);
		assert_eq!(balance(pallet_account, token_2.clone()), liquidity2 + input_amount);
	});
}

#[test]
fn can_swap_with_realistic_values() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let dot = NativeOrWithId::Native;
		let usd = NativeOrWithId::WithId(2);
		create_tokens(user, vec![usd.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(dot.clone()),
			Box::new(usd.clone())
		));

		const UNIT: u128 = 1_000_000_000;

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 300_000 * UNIT));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1_100_000 * UNIT));

		let liquidity_dot = 200_000 * UNIT; // ratio for a 5$ price
		let liquidity_usd = 1_000_000 * UNIT;
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(dot.clone()),
			Box::new(usd.clone()),
			liquidity_dot,
			liquidity_usd,
			1,
			1,
			user,
		));

		let input_amount = 10 * UNIT; // usd

		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![usd.clone(), dot.clone()],
			input_amount,
			1,
			user,
			false,
		));

		assert!(events().contains(&Event::<Test>::SwapExecuted {
			who: user,
			send_to: user,
			amount_in: 10 * UNIT,      // usd
			amount_out: 1_993_980_120, // About 2 dot after div by UNIT.
			path: vec![(usd, 10 * UNIT), (dot, 1_993_980_120)],
		}));
	});
}

#[test]
fn can_not_swap_in_pool_with_no_liquidity_added_yet() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		// Check can't swap an empty pool
		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				10,
				1,
				user,
				false,
			),
			Error::<Test>::PoolEmpty
		);
	});
}

#[test]
fn check_no_panic_when_try_swap_close_to_empty_pool() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());
		let lp_token = AssetConversion::get_next_pool_asset_id();

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let lp_token_minted = pool_balance(user, lp_token);
		assert!(events().contains(&Event::<Test>::LiquidityAdded {
			who: user,
			mint_to: user,
			pool_id: pool_id.clone(),
			amount1_provided: liquidity1,
			amount2_provided: liquidity2,
			lp_token,
			lp_token_minted,
		}));

		let pallet_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		assert_eq!(balance(pallet_account, token_1.clone()), liquidity1);
		assert_eq!(balance(pallet_account, token_2.clone()), liquidity2);

		assert_ok!(AssetConversion::remove_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			lp_token_minted,
			1,
			1,
			user,
		));

		// Now, the pool should exist but be almost empty.
		// Let's try and drain it.
		assert_eq!(balance(pallet_account, token_1.clone()), 708);
		assert_eq!(balance(pallet_account, token_2.clone()), 15);

		// validate the reserve should always stay above the ED
		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				708 - ed + 1, // amount_out
				500,          // amount_in_max
				user,
				false,
			),
			TokenError::NotExpendable,
		);

		assert_ok!(AssetConversion::swap_tokens_for_exact_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_2.clone(), token_1.clone()],
			608, // amount_out
			500, // amount_in_max
			user,
			false,
		));

		let token_1_left = balance(pallet_account, token_1.clone());
		let token_2_left = balance(pallet_account, token_2.clone());
		assert_eq!(token_1_left, 708 - 608);

		// The price for the last tokens should be very high
		assert_eq!(
			AssetConversion::get_amount_in(
				LpFee::get(),
				&(token_1_left - 1),
				&token_2_left,
				&token_1_left
			)
			.ok()
			.unwrap(),
			10625
		);

		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				token_1_left - 1, // amount_out
				1000,             // amount_in_max
				user,
				false,
			),
			Error::<Test>::ProvidedMaximumNotSufficientForSwap
		);

		// Try to swap what's left in the pool
		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				token_1_left, // amount_out
				1000,         // amount_in_max
				user,
				false,
			),
			Error::<Test>::AmountOutTooHigh
		);
	});
}

#[test]
fn swap_should_not_work_if_too_much_slippage() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(
			RuntimeOrigin::root(),
			user,
			10000 + get_native_ed()
		));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let exchange_amount = 100;

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				exchange_amount, // amount_in
				4000,            // amount_out_min
				user,
				false,
			),
			Error::<Test>::ProvidedMinimumNotSufficientForSwap
		);
	});
}

#[test]
fn can_swap_tokens_for_exact_tokens() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let pallet_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		let before1 = balance(pallet_account, token_1.clone()) + balance(user, token_1.clone());
		let before2 = balance(pallet_account, token_2.clone()) + balance(user, token_2.clone());

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let exchange_out = 50;
		let expect_in =
			AssetConversion::get_amount_in(LpFee::get(), &exchange_out, &liquidity1, &liquidity2)
				.ok()
				.unwrap();

		assert_ok!(AssetConversion::swap_tokens_for_exact_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_1.clone(), token_2.clone()],
			exchange_out, // amount_out
			3500,         // amount_in_max
			user,
			true,
		));

		assert_eq!(balance(user, token_1.clone()), 10000 + ed - expect_in);
		assert_eq!(balance(user, token_2.clone()), 1000 - liquidity2 + exchange_out);
		assert_eq!(balance(pallet_account, token_1.clone()), liquidity1 + expect_in);
		assert_eq!(balance(pallet_account, token_2.clone()), liquidity2 - exchange_out);

		// check invariants:

		// native and asset totals should be preserved.
		assert_eq!(
			before1,
			balance(pallet_account, token_1.clone()) + balance(user, token_1.clone())
		);
		assert_eq!(
			before2,
			balance(pallet_account, token_2.clone()) + balance(user, token_2.clone())
		);
	});
}

#[test]
fn can_swap_tokens_for_exact_tokens_when_not_liquidity_provider() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());
		let lp_token = AssetConversion::get_next_pool_asset_id();

		create_tokens(user2, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		let base1 = 10000;
		let base2 = 1000;
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, base1 + ed));
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, base1 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 2, user2, base2));

		let pallet_account = <Test as Config>::PoolLocator::address(&pool_id).unwrap();
		let before1 = balance(pallet_account, token_1.clone()) +
			balance(user, token_1.clone()) +
			balance(user2, token_1.clone());
		let before2 = balance(pallet_account, token_2.clone()) +
			balance(user, token_2.clone()) +
			balance(user2, token_2.clone());

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user2,
		));

		assert_eq!(balance(user, token_1.clone()), base1 + ed);
		assert_eq!(balance(user, token_2.clone()), 0);

		let exchange_out = 50;
		let expect_in =
			AssetConversion::get_amount_in(LpFee::get(), &exchange_out, &liquidity1, &liquidity2)
				.ok()
				.unwrap();

		assert_ok!(AssetConversion::swap_tokens_for_exact_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_1.clone(), token_2.clone()],
			exchange_out, // amount_out
			3500,         // amount_in_max
			user,
			true,
		));

		assert_eq!(balance(user, token_1.clone()), base1 + ed - expect_in);
		assert_eq!(balance(pallet_account, token_1.clone()), liquidity1 + expect_in);
		assert_eq!(balance(user, token_2.clone()), exchange_out);
		assert_eq!(balance(pallet_account, token_2.clone()), liquidity2 - exchange_out);

		// check invariants:

		// native and asset totals should be preserved.
		assert_eq!(
			before1,
			balance(pallet_account, token_1.clone()) +
				balance(user, token_1.clone()) +
				balance(user2, token_1.clone())
		);
		assert_eq!(
			before2,
			balance(pallet_account, token_2.clone()) +
				balance(user, token_2.clone()) +
				balance(user2, token_2.clone())
		);

		let lp_token_minted = pool_balance(user2, lp_token);
		assert_eq!(lp_token_minted, 1314);

		assert_ok!(AssetConversion::remove_liquidity(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			lp_token_minted,
			0,
			0,
			user2,
		));
	});
}

#[test]
fn swap_when_existential_deposit_would_cause_reaping_but_keep_alive_set() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user2, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 101));
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, 10000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 2, user2, 1000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 2, user, 2));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			200,
			1,
			1,
			user2,
		));

		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone()],
				1,   // amount_out
				101, // amount_in_max
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone()],
				51, // amount_in
				1,  // amount_out_min
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				51, // amount_out
				2,  // amount_in_max
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_1.clone()],
				2, // amount_in
				1, // amount_out_min
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);
	});
}

#[test]
fn swap_when_existential_deposit_would_cause_reaping_pool_account() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);

		let ed_assets = 100;
		create_tokens_with_ed(user2, vec![token_2.clone(), token_3.clone()], ed_assets);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_3.clone())
		));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user2),
			Box::new(token_2.clone()),
			Box::new(token_3.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 2, user2, 400 + ed_assets));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 3, user2, 20000 + ed_assets));

		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 2, user, 400 + ed_assets));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user2), 3, user, 20000 + ed_assets));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			200,
			1,
			1,
			user2,
		));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user2),
			Box::new(token_1.clone()),
			Box::new(token_3.clone()),
			200,
			10000,
			1,
			1,
			user2,
		));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user2),
			Box::new(token_2.clone()),
			Box::new(token_3.clone()),
			200,
			10000,
			1,
			1,
			user2,
		));

		// causes an account removal for asset token 2
		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone()],
				110,   // amount_out
				20000, // amount_in_max
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		// causes an account removal for asset token 2
		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone()],
				15000, // amount_in
				110,   // amount_out_min
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		// causes an account removal for native token 1
		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_3.clone(), token_1.clone()],
				110,   // amount_out
				20000, // amount_in_max
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		// causes an account removal for native token 1
		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_3.clone(), token_1.clone()],
				15000, // amount_in
				110,   // amount_out_min
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		// causes an account removal for native token 1 locate in the middle of a swap path
		let amount_in = AssetConversion::balance_path_from_amount_out(
			110,
			vec![token_3.clone(), token_1.clone()],
		)
		.unwrap()
		.first()
		.map(|(_, a)| *a)
		.unwrap();

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_3.clone(), token_1.clone(), token_2.clone()],
				amount_in, // amount_in
				1,         // amount_out_min
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);

		// causes an account removal for asset token 2 locate in the middle of a swap path
		let amount_in = AssetConversion::balance_path_from_amount_out(
			110,
			vec![token_1.clone(), token_2.clone()],
		)
		.unwrap()
		.first()
		.map(|(_, a)| *a)
		.unwrap();

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone(), token_3.clone()],
				amount_in, // amount_in
				1,         // amount_out_min
				user,
				true,
			),
			DispatchError::Token(TokenError::NotExpendable)
		);
	});
}

#[test]
fn swap_tokens_for_exact_tokens_should_not_work_if_too_much_slippage() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		assert_ok!(Balances::force_set_balance(
			RuntimeOrigin::root(),
			user,
			20000 + get_native_ed()
		));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let exchange_out = 1;

		assert_noop!(
			AssetConversion::swap_tokens_for_exact_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone()],
				exchange_out, // amount_out
				50,           // amount_in_max just greater than slippage.
				user,
				true
			),
			Error::<Test>::ProvidedMaximumNotSufficientForSwap
		);
	});
}

#[test]
fn swap_exact_tokens_for_tokens_in_multi_hops() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);

		create_tokens(user, vec![token_2.clone(), token_3.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_3.clone())
		));

		let ed = get_native_ed();
		let base1 = 10000;
		let base2 = 10000;
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, base1 * 2 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, base2));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, base2));

		let liquidity1 = 10000;
		let liquidity2 = 200;
		let liquidity3 = 2000;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_3.clone()),
			liquidity2,
			liquidity3,
			1,
			1,
			user,
		));

		let input_amount = 500;
		let expect_out2 =
			AssetConversion::get_amount_out(LpFee::get(), &input_amount, &liquidity1, &liquidity2)
				.ok()
				.unwrap();
		let expect_out3 =
			AssetConversion::get_amount_out(LpFee::get(), &expect_out2, &liquidity2, &liquidity3)
				.ok()
				.unwrap();

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone()],
				input_amount,
				80,
				user,
				true,
			),
			Error::<Test>::InvalidPath
		);

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_2.clone(), token_3.clone(), token_2.clone()],
				input_amount,
				80,
				user,
				true,
			),
			Error::<Test>::NonUniquePath
		);

		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_1.clone(), token_2.clone(), token_3.clone()],
			input_amount, // amount_in
			80,           // amount_out_min
			user,
			true,
		));

		let pool_id1 = (token_1.clone(), token_2.clone());
		let pool_id2 = (token_2.clone(), token_3.clone());
		let pallet_account1 = <Test as Config>::PoolLocator::address(&pool_id1).unwrap();
		let pallet_account2 = <Test as Config>::PoolLocator::address(&pool_id2).unwrap();

		assert_eq!(balance(user, token_1.clone()), base1 + ed - input_amount);
		assert_eq!(balance(pallet_account1, token_1.clone()), liquidity1 + input_amount);
		assert_eq!(balance(pallet_account1, token_2.clone()), liquidity2 - expect_out2);
		assert_eq!(balance(pallet_account2, token_2.clone()), liquidity2 + expect_out2);
		assert_eq!(balance(pallet_account2, token_3.clone()), liquidity3 - expect_out3);
		assert_eq!(balance(user, token_3.clone()), 10000 - liquidity3 + expect_out3);
	});
}

#[test]
fn swap_tokens_for_exact_tokens_in_multi_hops() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);

		create_tokens(user, vec![token_2.clone(), token_3.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_3.clone())
		));

		let ed = get_native_ed();
		let base1 = 10000;
		let base2 = 10000;
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, base1 * 2 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, base2));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, base2));

		let liquidity1 = 10000;
		let liquidity2 = 200;
		let liquidity3 = 2000;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_2.clone()),
			Box::new(token_3.clone()),
			liquidity2,
			liquidity3,
			1,
			1,
			user,
		));

		let exchange_out3 = 100;
		let expect_in2 =
			AssetConversion::get_amount_in(LpFee::get(), &exchange_out3, &liquidity2, &liquidity3)
				.ok()
				.unwrap();
		let expect_in1 =
			AssetConversion::get_amount_in(LpFee::get(), &expect_in2, &liquidity1, &liquidity2)
				.ok()
				.unwrap();

		assert_ok!(AssetConversion::swap_tokens_for_exact_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_1.clone(), token_2.clone(), token_3.clone()],
			exchange_out3, // amount_out
			1000,          // amount_in_max
			user,
			true,
		));

		let pool_id1 = (token_1.clone(), token_2.clone());
		let pool_id2 = (token_2.clone(), token_3.clone());
		let pallet_account1 = <Test as Config>::PoolLocator::address(&pool_id1).unwrap();
		let pallet_account2 = <Test as Config>::PoolLocator::address(&pool_id2).unwrap();

		assert_eq!(balance(user, token_1.clone()), base1 + ed - expect_in1);
		assert_eq!(balance(pallet_account1, token_1.clone()), liquidity1 + expect_in1);
		assert_eq!(balance(pallet_account1, token_2.clone()), liquidity2 - expect_in2);
		assert_eq!(balance(pallet_account2, token_2.clone()), liquidity2 + expect_in2);
		assert_eq!(balance(pallet_account2, token_3.clone()), liquidity3 - exchange_out3);
		assert_eq!(balance(user, token_3.clone()), 10000 - liquidity3 + exchange_out3);
	});
}

#[test]
fn can_not_swap_same_asset() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::WithId(1);
		let token_2 = NativeOrWithId::Native;

		create_tokens(user, vec![token_1.clone()]);
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 1, user, 1000));

		let liquidity1 = 1000;
		let liquidity2 = 20;
		assert_noop!(
			AssetConversion::add_liquidity(
				RuntimeOrigin::signed(user),
				Box::new(token_1.clone()),
				Box::new(token_1.clone()),
				liquidity1,
				liquidity2,
				1,
				1,
				user,
			),
			Error::<Test>::InvalidAssetPair
		);

		let exchange_amount = 10;
		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_1.clone(), token_1.clone()],
				exchange_amount,
				1,
				user,
				true,
			),
			Error::<Test>::InvalidAssetPair
		);

		assert_noop!(
			AssetConversion::swap_exact_tokens_for_tokens(
				RuntimeOrigin::signed(user),
				bvec![token_2.clone(), token_2.clone()],
				exchange_amount,
				1,
				user,
				true,
			),
			Error::<Test>::InvalidAssetPair
		);
	});
}

#[test]
fn cannot_block_pool_creation() {
	new_test_ext().execute_with(|| {
		// User 1 is the pool creator
		let user = 1;
		// User 2 is the attacker
		let attacker = 2;

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), attacker, 10000 + ed));

		// The target pool the user wants to create is Native <=> WithId(2)
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		// Attacker computes the still non-existing pool account for the target pair
		let pool_account =
			<Test as Config>::PoolLocator::address(&(token_1.clone(), token_2.clone())).unwrap();
		// And transfers the ED to that pool account
		assert_ok!(Balances::transfer_allow_death(
			RuntimeOrigin::signed(attacker),
			pool_account,
			ed
		));
		// Then, the attacker creates 14 tokens and sends one of each to the pool account
		for i in 10..25 {
			create_tokens(attacker, vec![NativeOrWithId::WithId(i)]);
			assert_ok!(Assets::mint(RuntimeOrigin::signed(attacker), i, attacker, 1000));
			assert_ok!(Assets::transfer(RuntimeOrigin::signed(attacker), i, pool_account, 1));
		}

		// User can still create the pool
		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		// User has to transfer one WithId(2) token to the pool account (otherwise add_liquidity
		// will fail with `AssetTwoDepositDidNotMeetMinimum`)
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 10000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 10000));
		assert_ok!(Assets::transfer(RuntimeOrigin::signed(user), 2, pool_account, 1));

		// add_liquidity shouldn't fail because of the number of consumers
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			10000,
			100,
			10000,
			10,
			user,
		));
	});
}

#[test]
fn swap_transactional() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_3 = NativeOrWithId::WithId(3);

		let asset_ed = 150;
		create_tokens_with_ed(user, vec![token_2.clone(), token_3.clone()], asset_ed);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_3.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user, 1000));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user2, 1000));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 3, user2, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_3.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let pool_1 =
			<Test as Config>::PoolLocator::address(&(token_1.clone(), token_2.clone())).unwrap();
		let pool_2 =
			<Test as Config>::PoolLocator::address(&(token_1.clone(), token_3.clone())).unwrap();

		assert_eq!(Balances::balance(&pool_1), liquidity1);
		assert_eq!(Assets::balance(2, pool_1), liquidity2);
		assert_eq!(Balances::balance(&pool_2), liquidity1);
		assert_eq!(Assets::balance(3, pool_2), liquidity2);

		// the amount that would cause a transfer from the last pool in the path to fail
		let expected_out = liquidity2 - asset_ed + 1;
		let amount_in = AssetConversion::balance_path_from_amount_out(
			expected_out,
			vec![token_2.clone(), token_1.clone(), token_3.clone()],
		)
		.unwrap()
		.first()
		.map(|(_, a)| *a)
		.unwrap();

		// swap credit with `swap_tokens_for_exact_tokens` transactional
		let credit_in = NativeAndAssets::issue(token_2.clone(), amount_in);
		let credit_in_err_expected = NativeAndAssets::issue(token_2.clone(), amount_in);
		// avoiding drop of any credit, to assert any storage mutation from an actual call.
		let error;
		assert_storage_noop!(
			error = <AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
				vec![token_2.clone(), token_1.clone(), token_3.clone()],
				credit_in,
				expected_out,
			)
			.unwrap_err()
		);
		assert_eq!(error, (credit_in_err_expected, TokenError::NotExpendable.into()));

		// swap credit with `swap_exact_tokens_for_tokens` transactional
		let credit_in = NativeAndAssets::issue(token_2.clone(), amount_in);
		let credit_in_err_expected = NativeAndAssets::issue(token_2.clone(), amount_in);
		// avoiding drop of any credit, to assert any storage mutation from an actual call.
		let error;
		assert_storage_noop!(
			error = <AssetConversion as SwapCredit<_>>::swap_exact_tokens_for_tokens(
				vec![token_2.clone(), token_1.clone(), token_3.clone()],
				credit_in,
				Some(expected_out),
			)
			.unwrap_err()
		);
		assert_eq!(error, (credit_in_err_expected, TokenError::NotExpendable.into()));

		// swap with `swap_exact_tokens_for_tokens` transactional
		assert_noop!(
			<AssetConversion as Swap<_>>::swap_exact_tokens_for_tokens(
				user2,
				vec![token_2.clone(), token_1.clone(), token_3.clone()],
				amount_in,
				Some(expected_out),
				user2,
				true,
			),
			TokenError::NotExpendable
		);

		// swap with `swap_exact_tokens_for_tokens` transactional
		assert_noop!(
			<AssetConversion as Swap<_>>::swap_tokens_for_exact_tokens(
				user2,
				vec![token_2.clone(), token_1.clone(), token_3.clone()],
				expected_out,
				Some(amount_in),
				user2,
				true,
			),
			TokenError::NotExpendable
		);

		assert_eq!(Balances::balance(&pool_1), liquidity1);
		assert_eq!(Assets::balance(2, pool_1), liquidity2);
		assert_eq!(Balances::balance(&pool_2), liquidity1);
		assert_eq!(Assets::balance(3, pool_2), liquidity2);
	})
}

#[test]
fn swap_credit_returns_change() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		let expected_change = NativeAndAssets::issue(token_1.clone(), 100);
		let expected_credit_out = NativeAndAssets::issue(token_2.clone(), 20);

		let amount_in_max = AssetConversion::get_amount_in(
			LpFee::get(),
			&expected_credit_out.peek(),
			&liquidity1,
			&liquidity2,
		)
		.unwrap();

		let credit_in =
			NativeAndAssets::issue(token_1.clone(), amount_in_max + expected_change.peek());
		assert_ok!(
			<AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
				vec![token_1.clone(), token_2.clone()],
				credit_in,
				expected_credit_out.peek(),
			),
			(expected_credit_out, expected_change)
		);
	})
}

#[test]
fn swap_credit_insufficient_amount_bounds() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user2, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		// provided `credit_in` is not sufficient to swap for desired `amount_out_min`
		let amount_out_min = 20;
		let amount_in = AssetConversion::get_amount_in(
			LpFee::get(),
			&(amount_out_min - 1),
			&liquidity2,
			&liquidity1,
		)
		.unwrap();
		let credit_in = NativeAndAssets::issue(token_1.clone(), amount_in);
		let expected_credit_in = NativeAndAssets::issue(token_1.clone(), amount_in);
		let error = <AssetConversion as SwapCredit<_>>::swap_exact_tokens_for_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			Some(amount_out_min),
		)
		.unwrap_err();
		assert_eq!(
			error,
			(expected_credit_in, Error::<Test>::ProvidedMinimumNotSufficientForSwap.into())
		);

		// provided `credit_in` is not sufficient to swap for desired `amount_out`
		let amount_out = 20;
		let amount_in_max = AssetConversion::get_amount_in(
			LpFee::get(),
			&(amount_out - 1),
			&liquidity2,
			&liquidity1,
		)
		.unwrap();
		let credit_in = NativeAndAssets::issue(token_1.clone(), amount_in_max);
		let expected_credit_in = NativeAndAssets::issue(token_1.clone(), amount_in_max);
		let error = <AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			amount_out,
		)
		.unwrap_err();
		assert_eq!(
			error,
			(expected_credit_in, Error::<Test>::ProvidedMaximumNotSufficientForSwap.into())
		);
	})
}

#[test]
fn swap_credit_zero_amount() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user2, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		// swap with zero credit fails for `swap_exact_tokens_for_tokens`
		let credit_in = CreditOf::<Test>::zero(token_1.clone());
		let expected_credit_in = CreditOf::<Test>::zero(token_1.clone());
		let error = <AssetConversion as SwapCredit<_>>::swap_exact_tokens_for_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			None,
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::ZeroAmount.into()));

		// swap with zero credit fails for `swap_tokens_for_exact_tokens`
		let credit_in = CreditOf::<Test>::zero(token_1.clone());
		let expected_credit_in = CreditOf::<Test>::zero(token_1.clone());
		let error = <AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			10,
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::ZeroAmount.into()));

		// swap with zero amount_out_min fails for `swap_exact_tokens_for_tokens`
		let credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let expected_credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let error = <AssetConversion as SwapCredit<_>>::swap_exact_tokens_for_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			Some(0),
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::ZeroAmount.into()));

		// swap with zero amount_out fails with `swap_tokens_for_exact_tokens` fails
		let credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let expected_credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let error = <AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			0,
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::ZeroAmount.into()));
	});
}

#[test]
fn swap_credit_invalid_path() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let user2 = 2;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user2, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user2, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		// swap with credit_in.asset different from path[0] asset fails
		let credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let expected_credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let error = <AssetConversion as SwapCredit<_>>::swap_exact_tokens_for_tokens(
			vec![token_2.clone(), token_1.clone()],
			credit_in,
			None,
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::InvalidPath.into()));

		// swap with credit_in.asset different from path[0] asset fails
		let credit_in = NativeAndAssets::issue(token_2.clone(), 10);
		let expected_credit_in = NativeAndAssets::issue(token_2.clone(), 10);
		let error = <AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			10,
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::InvalidPath.into()));

		// swap with path.len < 2 fails
		let credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let expected_credit_in = NativeAndAssets::issue(token_1.clone(), 10);
		let error = <AssetConversion as SwapCredit<_>>::swap_exact_tokens_for_tokens(
			vec![token_2.clone()],
			credit_in,
			None,
		)
		.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::InvalidPath.into()));

		// swap with path.len < 2 fails
		let credit_in = NativeAndAssets::issue(token_2.clone(), 10);
		let expected_credit_in = NativeAndAssets::issue(token_2.clone(), 10);
		let error =
			<AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(vec![], credit_in, 10)
				.unwrap_err();
		assert_eq!(error, (expected_credit_in, Error::<Test>::InvalidPath.into()));
	});
}

#[test]
fn swap_credit_returns_change_when_swap_fails() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let token_2_ed = 150;

		create_tokens_with_ed(user, vec![token_2.clone()], token_2_ed);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone())
		));

		let ed = get_native_ed();
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), user, 20000 + ed));
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;

		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		// The path prices fine, but taking `amount_out` from the pool would leave it below the
		// minimum balance of `token_2`, so the swap fails after `credit_in` was split.
		let amount_out = liquidity2 - token_2_ed + 1;
		let amount_in =
			AssetConversion::get_amount_in(LpFee::get(), &amount_out, &liquidity1, &liquidity2)
				.unwrap();
		let change = 100;

		let total_issuance = Balances::total_issuance();
		let credit_in = NativeAndAssets::issue(token_1.clone(), amount_in + change);
		let (credit_in, error) = <AssetConversion as SwapCredit<_>>::swap_tokens_for_exact_tokens(
			vec![token_1.clone(), token_2.clone()],
			credit_in,
			amount_out,
		)
		.unwrap_err();
		assert_eq!(error, TokenError::NotExpendable.into());
		// The whole `credit_in` comes back, including the change.
		assert_eq!(credit_in.peek(), amount_in + change);
		drop(credit_in);
		assert_eq!(Balances::total_issuance(), total_issuance);
	});
}

fn new_test_ext_with_genesis_pools(
	pools: Vec<(NativeOrWithId<u32>, NativeOrWithId<u32>, u128, u128, u128)>,
) -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();

	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(1, 10000), (2, 20000)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();

	pallet_assets::GenesisConfig::<Test, Instance1> {
		assets: vec![(1, 0, true, 1)],
		accounts: vec![(1, 1, 10000), (1, 2, 10000)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();

	crate::GenesisConfig::<Test> { pools }.assimilate_storage(&mut t).unwrap();

	let mut ext = sp_io::TestExternalities::new(t);
	ext.execute_with(|| System::set_block_number(1));
	ext
}

#[test]
fn genesis_pool_with_liquidity() {
	use NativeOrWithId::{Native, WithId};
	new_test_ext_with_genesis_pools(vec![(Native, WithId(1), 1, 1000, 2000)]).execute_with(|| {
		assert_eq!(pools(), vec![(Native, WithId(1))]);
		assert_eq!(pool_assets(), vec![0]);
		// Provider's native balance decreased by pool contribution.
		assert_eq!(balance(1, Native), 10000 - 1000);
		// Provider's asset balance decreased by pool contribution.
		assert_eq!(balance(1, WithId(1)), 10000 - 2000);
		// Provider received LP tokens.
		assert!(pool_balance(1, 0) > 0);
		// No pool setup fee charged (balance is exactly 10000 - 1000).
	});
}

#[test]
fn genesis_pool_without_liquidity() {
	use NativeOrWithId::{Native, WithId};
	new_test_ext_with_genesis_pools(vec![(Native, WithId(1), 1, 0, 0)]).execute_with(|| {
		assert_eq!(pools(), vec![(Native, WithId(1))]);
		assert_eq!(pool_assets(), vec![0]);
		assert_eq!(balance(1, Native), 10000);
		assert_eq!(balance(1, WithId(1)), 10000);
		assert_eq!(pool_balance(1, 0), 0);
	});
}

#[test]
#[should_panic(expected = "PoolExists")]
fn genesis_duplicate_pool_panics() {
	use NativeOrWithId::{Native, WithId};
	new_test_ext_with_genesis_pools(vec![
		(Native, WithId(1), 1, 1000, 2000),
		(Native, WithId(1), 1, 1000, 2000),
	]);
}

#[test]
#[should_panic(expected = "InvalidAssetPair")]
fn genesis_same_assets_panics() {
	use NativeOrWithId::WithId;
	new_test_ext_with_genesis_pools(vec![(WithId(1), WithId(1), 1, 1000, 2000)]);
}

#[test]
fn pool_fee_falls_back_to_global_lp_fee() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());

		create_tokens(user, vec![token_2.clone()]);
		Balances::force_set_balance(RuntimeOrigin::root(), user, 1000).unwrap();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1),
			Box::new(token_2),
		));

		// No override set, so the global `LPFee` applies.
		assert_eq!(PoolFees::<Test>::get(&pool_id), None);
		assert_eq!(AssetConversion::pool_fee(&pool_id), <Test as Config>::LPFee::get());
	});
}

#[test]
fn create_pool_with_fee_sets_override() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());
		let fee = Permill::from_percent(1);

		create_tokens(user, vec![token_2.clone()]);
		Balances::force_set_balance(RuntimeOrigin::root(), user, 1000).unwrap();

		// A signed (non-admin) origin cannot create a pool with a custom fee.
		assert_noop!(
			AssetConversion::create_pool_with_fee(
				RuntimeOrigin::signed(user),
				user,
				Box::new(token_1.clone()),
				Box::new(token_2.clone()),
				fee,
			),
			DispatchError::BadOrigin,
		);

		// A fee above `MaxSwapFee` is rejected.
		assert_noop!(
			AssetConversion::create_pool_with_fee(
				RuntimeOrigin::root(),
				user,
				Box::new(token_1.clone()),
				Box::new(token_2.clone()),
				MaxSwapFee::get() + Permill::from_percent(1),
			),
			Error::<Test>::FeeTooHigh,
		);

		assert_ok!(AssetConversion::create_pool_with_fee(
			RuntimeOrigin::root(),
			user,
			Box::new(token_1),
			Box::new(token_2),
			fee,
		));

		assert_eq!(PoolFees::<Test>::get(&pool_id), Some(fee));
		assert_eq!(AssetConversion::pool_fee(&pool_id), fee);

		// Both `PoolCreated` and `PoolFeeSet` are emitted, in that order.
		assert!(events().iter().any(|e| matches!(
			e,
			Event::PoolFeeSet { pool_id: p, fee: f } if *p == pool_id && *f == fee
		)));
	});
}

#[test]
fn set_pool_fee_requires_admin_origin_and_overrides() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());
		let fee = Permill::from_percent(5);

		create_tokens(user, vec![token_2.clone()]);
		Balances::force_set_balance(RuntimeOrigin::root(), user, 1000).unwrap();
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1),
			Box::new(token_2),
		));

		// A signed (non-admin) origin cannot set the fee.
		assert_noop!(
			AssetConversion::set_pool_fee(RuntimeOrigin::signed(user), pool_id.clone(), fee),
			DispatchError::BadOrigin,
		);
		assert_eq!(PoolFees::<Test>::get(&pool_id), None);

		// A fee above `MaxSwapFee` is rejected.
		assert_noop!(
			AssetConversion::set_pool_fee(
				RuntimeOrigin::root(),
				pool_id.clone(),
				MaxSwapFee::get() + Permill::from_percent(1),
			),
			Error::<Test>::FeeTooHigh,
		);

		// The admin origin (root) can.
		assert_ok!(AssetConversion::set_pool_fee(RuntimeOrigin::root(), pool_id.clone(), fee));
		assert_eq!(PoolFees::<Test>::get(&pool_id), Some(fee));
		assert_eq!(AssetConversion::pool_fee(&pool_id), fee);
		assert!(events().iter().any(|e| matches!(
			e,
			Event::PoolFeeSet { pool_id: p, fee: f } if *p == pool_id && *f == fee
		)));
	});
}

#[test]
fn set_pool_fee_on_missing_pool_fails() {
	new_test_ext().execute_with(|| {
		let pool_id = (NativeOrWithId::Native, NativeOrWithId::WithId(2));
		assert_noop!(
			AssetConversion::set_pool_fee(RuntimeOrigin::root(), pool_id, Permill::from_percent(1),),
			Error::<Test>::PoolNotFound,
		);
	});
}

#[test]
fn per_pool_fee_is_applied_to_swaps() {
	new_test_ext().execute_with(|| {
		let user = 1;
		let token_1 = NativeOrWithId::Native;
		let token_2 = NativeOrWithId::WithId(2);
		let pool_id = (token_1.clone(), token_2.clone());
		let fee = Permill::from_percent(10);

		create_tokens(user, vec![token_2.clone()]);
		assert_ok!(AssetConversion::create_pool(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
		));

		let ed = get_native_ed();
		Balances::force_set_balance(RuntimeOrigin::root(), user, 10000 + ed).unwrap();
		assert_ok!(Assets::mint(RuntimeOrigin::signed(user), 2, user, 1000));

		let liquidity1 = 10000;
		let liquidity2 = 200;
		assert_ok!(AssetConversion::add_liquidity(
			RuntimeOrigin::signed(user),
			Box::new(token_1.clone()),
			Box::new(token_2.clone()),
			liquidity1,
			liquidity2,
			1,
			1,
			user,
		));

		// Override the pool's fee to 10% (vs. the 0.3% global default).
		assert_ok!(AssetConversion::set_pool_fee(RuntimeOrigin::root(), pool_id, fee));

		let input_amount = 100;
		// Output is computed with the per-pool fee, not the global one.
		let with_pool_fee =
			AssetConversion::get_amount_out(fee, &input_amount, &liquidity2, &liquidity1).unwrap();
		let with_global_fee =
			AssetConversion::get_amount_out(LpFee::get(), &input_amount, &liquidity2, &liquidity1)
				.unwrap();
		assert!(with_pool_fee < with_global_fee, "higher fee must yield less output");

		// The quote endpoint reflects the override.
		assert_eq!(
			AssetConversion::quote_price_exact_tokens_for_tokens(
				token_2.clone(),
				token_1.clone(),
				input_amount,
				true,
			),
			Some(with_pool_fee),
		);

		// And the actual swap pays out exactly the per-pool-fee amount.
		assert_ok!(AssetConversion::swap_exact_tokens_for_tokens(
			RuntimeOrigin::signed(user),
			bvec![token_2.clone(), token_1.clone()],
			input_amount,
			1,
			user,
			false,
		));
		assert_eq!(balance(user, token_1), with_pool_fee + ed);
	});
}
