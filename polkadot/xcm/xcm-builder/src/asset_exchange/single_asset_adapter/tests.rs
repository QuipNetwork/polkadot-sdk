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

//! Tests for the [`SingleAssetExchangeAdapter`] type.

use super::mock::*;
use crate::tests::mock::assets_to_holding;
use frame_support::traits::fungible::Inspect;
use xcm::prelude::*;
use xcm_executor::{traits::AssetExchange, AssetsInHolding, XcmExecutor};

// ========== Happy path ==========

/// Scenario:
/// Account #3 wants to use the local liquidity pool between two custom assets,
/// 1 and 2.
#[test]
fn maximal_exchange() {
	new_test_ext().execute_with(|| {
		let assets = PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![([PalletInstance(2), GeneralIndex(1)], 10_000_000).into()]),
			&vec![(Here, 2_000_000).into()].into(),
			true, // Maximal
		)
		.unwrap();
		let amount = get_amount_from_first_fungible(&assets);
		assert_eq!(amount, 4_533_054);
	});
}

#[test]
fn minimal_exchange() {
	new_test_ext().execute_with(|| {
		let assets = PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![([PalletInstance(2), GeneralIndex(1)], 10_000_000).into()]),
			&vec![(Here, 2_000_000).into()].into(),
			false, // Minimal
		)
		.unwrap();
		let (first_amount, second_amount) = get_amount_from_fungibles(&assets);
		assert_eq!(first_amount, 2_000_000);
		assert_eq!(second_amount, 5_820_795);
	});
}

#[test]
fn maximal_quote() {
	new_test_ext().execute_with(|| {
		let assets = quote(
			&([PalletInstance(2), GeneralIndex(1)], 10_000_000).into(),
			&(Here, 2_000_000).into(),
			true,
		)
		.unwrap();
		let amount = get_amount_from_first_fungible(&assets_to_holding(assets.into_inner()));
		// The amount of the native token resulting from swapping all `10_000_000` of the custom
		// token.
		assert_eq!(amount, 4_533_054);
	});
}

#[test]
fn minimal_quote() {
	new_test_ext().execute_with(|| {
		let assets = quote(
			&([PalletInstance(2), GeneralIndex(1)], 10_000_000).into(),
			&(Here, 2_000_000).into(),
			false,
		)
		.unwrap();
		let amount = get_amount_from_first_fungible(&assets_to_holding(assets.into_inner()));
		// The amount of the custom token needed to get `2_000_000` of the native token.
		assert_eq!(amount, 4_179_205);
	});
}

// ========== Unhappy path ==========

#[test]
fn no_asset_in_give() {
	new_test_ext().execute_with(|| {
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![]),
			&vec![(Here, 2_000_000).into()].into(),
			true
		)
		.is_err());
	});
}

#[test]
fn more_than_one_asset_in_give() {
	new_test_ext().execute_with(|| {
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![
				([PalletInstance(2), GeneralIndex(1)], 1).into(),
				(Here, 2).into()
			]),
			&vec![(Here, 2_000_000).into()].into(),
			true
		)
		.is_err());
	});
}

#[test]
fn no_asset_in_want() {
	new_test_ext().execute_with(|| {
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![([PalletInstance(2), GeneralIndex(1)], 10_000_000).into()]),
			&vec![].into(),
			true
		)
		.is_err());
	});
}

#[test]
fn more_than_one_asset_in_want() {
	new_test_ext().execute_with(|| {
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![([PalletInstance(2), GeneralIndex(1)], 10_000_000).into()]),
			&vec![(Here, 2_000_000).into(), ([PalletInstance(2), GeneralIndex(1)], 1).into()]
				.into(),
			true
		)
		.is_err());
	});
}

#[test]
fn give_asset_does_not_match() {
	new_test_ext().execute_with(|| {
		let nonexistent_asset_id = 1000;
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![(
				[PalletInstance(2), GeneralIndex(nonexistent_asset_id)],
				10_000_000
			)
				.into()]),
			&vec![(Here, 2_000_000).into()].into(),
			true
		)
		.is_err());
	});
}

#[test]
fn want_asset_does_not_match() {
	new_test_ext().execute_with(|| {
		let nonexistent_asset_id = 1000;
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![(Here, 2_000_000).into()]),
			&vec![([PalletInstance(2), GeneralIndex(nonexistent_asset_id)], 10_000_000).into()]
				.into(),
			true
		)
		.is_err());
	});
}

#[test]
fn exchange_fails() {
	new_test_ext().execute_with(|| {
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			assets_to_holding(vec![([PalletInstance(2), GeneralIndex(1)], 10_000_000).into()]),
			// We're asking for too much of the native token...
			&vec![(Here, 200_000_000).into()].into(),
			false, // Minimal
		)
		.is_err());
	});
}

#[test]
fn non_fungible_asset_in_give() {
	new_test_ext().execute_with(|| {
		assert!(PoolAssetsExchanger::exchange_asset(
			None,
			// Using `u64` here will give us a non-fungible instead of a fungible.
			assets_to_holding(vec![([PalletInstance(2), GeneralIndex(2)], 10_000_000u64).into()]),
			&vec![(Here, 10_000_000).into()].into(),
			false, // Minimal
		)
		.is_err());
	});
}

// ========== Delivery fees ==========

/// Scenario:
/// Account #1 sends native tokens with `TransferReserveAsset` and pays the delivery fee (asset 1)
/// with native tokens, withdrawn from its account and swapped through the pool.
#[test]
fn transfer_reserve_asset_pays_delivery_fee_via_exchange() {
	new_test_ext().execute_with(|| {
		let total_issuance = Balances::total_issuance();
		transfer_reserve_asset_paying_fee_in_native();
		assert!(Balances::total_balance(&1) < INITIAL_BALANCE);
		assert_eq!(Balances::total_issuance(), total_issuance);
	});
}

/// Scenario:
/// Same as above, but the fee exchange fails. The whole instruction is rolled back, so account
/// #1's balance and the total issuance stay unchanged.
#[test]
fn transfer_reserve_asset_rolls_back_when_fee_exchange_fails() {
	set_fail_exchange(true);
	new_test_ext().execute_with(|| {
		let total_issuance = Balances::total_issuance();
		transfer_reserve_asset_paying_fee_in_native();
		assert_eq!(Balances::total_balance(&1), INITIAL_BALANCE);
		assert_eq!(Balances::total_issuance(), total_issuance);
	});
}

// ========== Helper functions ==========

fn get_amount_from_first_fungible(assets: &AssetsInHolding) -> u128 {
	let mut fungibles_iter = assets.fungible_assets_iter();
	let first_fungible = fungibles_iter.next().unwrap();
	let Fungible(amount) = first_fungible.fun else {
		unreachable!("Asset should be fungible");
	};
	amount
}

fn get_amount_from_fungibles(assets: &AssetsInHolding) -> (u128, u128) {
	let mut fungibles_iter = assets.fungible_assets_iter();
	let first_fungible = fungibles_iter.next().unwrap();
	let Fungible(first_amount) = first_fungible.fun else {
		unreachable!("Asset should be fungible");
	};
	let second_fungible = fungibles_iter.next().unwrap();
	let Fungible(second_amount) = second_fungible.fun else {
		unreachable!("Asset should be fungible");
	};
	(first_amount, second_amount)
}

fn quote(asset_1: &Asset, asset_2: &Asset, maximal: bool) -> Option<Assets> {
	PoolAssetsExchanger::quote_exchange_price(
		&asset_1.clone().into(),
		&asset_2.clone().into(),
		maximal,
	)
}

/// Account #1 sends 1 native token to account #5 with `TransferReserveAsset`. The delivery fee is
/// withdrawn from the account in native tokens (`jit_withdraw`) and swapped for asset 1.
fn transfer_reserve_asset_paying_fee_in_native() {
	let account: Location = AccountIndex64 { network: None, index: 1 }.into();
	let message = Xcm::<RuntimeCall>(vec![
		// Return whatever is left to the account, also when an instruction fails.
		SetAppendix(Xcm(vec![
			RefundSurplus,
			DepositAsset { assets: Wild(AllCounted(2)), beneficiary: account.clone() },
		])),
		WithdrawAsset((Here, 1u128).into()),
		// Makes native the asset used to pay delivery fees.
		BuyExecution {
			fees: (Here, 1u128).into(),
			weight_limit: Limited(Weight::from_parts(100, 100)),
		},
		SetFeesMode { jit_withdraw: true },
		TransferReserveAsset {
			assets: (Here, 1u128).into(),
			dest: AccountIndex64 { network: None, index: 5 }.into(),
			xcm: Xcm(vec![]),
		},
	]);
	let mut hash = [0u8; 32];
	let _ = XcmExecutor::<XcmConfig>::prepare_and_execute(
		account,
		message,
		&mut hash,
		Weight::from_parts(1_000, 1_000),
		Weight::zero(),
	);
}
