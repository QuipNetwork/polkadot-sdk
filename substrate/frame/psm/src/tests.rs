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

use super::mock::*;
use crate::{
	AssetCeilingWeight, CircuitBreakerLevel, Error, Event, ExternalAssets, MintingFee, PsmDebt,
	RedemptionFee,
};
use codec::{Decode, Encode};
use frame_support::{assert_noop, assert_ok, hypothetically};
use sp_runtime::{traits::TrailingZeroInput, DispatchError, Permill, TokenError, TypeId};

fn psm_max_debt() -> u128 {
	crate::Psm::<Test>::get(INTERNAL_ASSET_ID)
		.map(|p| p.max_debt)
		.unwrap_or_default()
}

fn psm_max_asset_debt(asset_id: u32) -> u128 {
	let info = crate::Psm::<Test>::get(INTERNAL_ASSET_ID).expect("PSM exists at genesis");
	crate::Pallet::<Test>::max_asset_debt(&INTERNAL_ASSET_ID, &asset_id, &info)
}

mod mint {
	use super::*;

	#[test]
	fn success_basic() {
		new_test_ext().execute_with(|| {
			let mint_amount = 1000 * INTERNAL_UNIT;
			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				mint_amount,
				Permill::from_percent(1)
			));

			let fee = Permill::from_percent(1).mul_ceil(mint_amount);
			let internal_to_user = mint_amount - fee;

			assert_eq!(get_asset_balance(USDC_ASSET_ID, ALICE), alice_usdc_before - mint_amount);
			assert_eq!(get_asset_balance(USDC_ASSET_ID, psm_account()), mint_amount);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), internal_to_user);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), fee);
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), mint_amount);

			System::assert_has_event(
				Event::<Test>::Minted {
					who: ALICE,
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
					external_consumed: mint_amount,
					internal_received: internal_to_user,
					internal_fee: fee,
				}
				.into(),
			);
		});
	}

	#[test]
	fn fee_zero() {
		new_test_ext().execute_with(|| {
			set_minting_fee(USDC_ASSET_ID, Permill::zero());

			let mint_amount = 1000 * INTERNAL_UNIT;

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				mint_amount,
				Permill::zero()
			));

			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), mint_amount);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), 0);
		});
	}

	#[test]
	fn fee_nonzero() {
		new_test_ext().execute_with(|| {
			set_minting_fee(USDC_ASSET_ID, Permill::from_percent(5));

			let mint_amount = 1000 * INTERNAL_UNIT;
			let fee = Permill::from_percent(5).mul_ceil(mint_amount);
			let internal_to_user = mint_amount - fee;

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				mint_amount,
				Permill::from_percent(5)
			));

			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), internal_to_user);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), fee);
		});
	}

	#[test]
	fn fee_100_percent() {
		new_test_ext().execute_with(|| {
			set_minting_fee(USDC_ASSET_ID, Permill::from_percent(100));

			let mint_amount = 1000 * INTERNAL_UNIT;

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				mint_amount,
				Permill::from_percent(100)
			));

			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), 0);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), mint_amount);
		});
	}

	#[test]
	fn fails_when_fee_exceeds_max_fee() {
		new_test_ext().execute_with(|| {
			set_minting_fee(USDC_ASSET_ID, Permill::from_percent(10));

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1),
				),
				Error::<Test>::FeeTooHigh
			);
		});
	}

	#[test]
	fn fails_unsupported_asset() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					UNSUPPORTED_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::zero()
				),
				Error::<Test>::UnsupportedAsset
			);
		});
	}

	#[test]
	fn fails_asset_minting_disabled() {
		new_test_ext().execute_with(|| {
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::MintingDisabled);

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::MintingStopped
			);

			// Other assets should still work
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
		});
	}

	#[test]
	fn fails_asset_all_disabled() {
		new_test_ext().execute_with(|| {
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::AllDisabled);

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::MintingStopped
			);

			// Other assets should still work
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
		});
	}

	#[test]
	fn fails_below_minimum() {
		new_test_ext().execute_with(|| {
			let below_min = MinSwapAmount::get() - 1;

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					below_min,
					Permill::from_percent(1)
				),
				Error::<Test>::BelowMinimumSwap
			);
		});
	}

	#[test]
	fn fails_exceeds_max_debt() {
		new_test_ext().execute_with(|| {
			// Set global ceiling to 1% and asset ratio to 100%
			set_max_debt(200_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(100));

			let max_debt = psm_max_asset_debt(USDC_ASSET_ID);
			let too_much = max_debt + 1;

			fund_external_asset(USDC_ASSET_ID, ALICE, too_much);

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					too_much,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}

	#[test]
	fn fails_with_extreme_debt_value() {
		new_test_ext().execute_with(|| {
			// When PSM debt is set to an extreme value, the aggregate ceiling check
			// will catch it before reaching the per-asset arithmetic overflow check.
			// This is correct behavior - ceiling checks provide safety.
			set_max_debt(20_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(100));

			PsmDebt::<Test>::insert(INTERNAL_ASSET_ID, USDC_ASSET_ID, u128::MAX - 100);

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}

	#[test]
	fn boundary_new_debt_equals_max() {
		new_test_ext().execute_with(|| {
			// Set USDC to 100% and USDT to 0% so USDC gets full ceiling
			set_max_debt(200_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(100));
			set_asset_ceiling_weight(USDT_ASSET_ID, Permill::from_percent(0));

			let max_debt = psm_max_asset_debt(USDC_ASSET_ID);

			fund_external_asset(USDC_ASSET_ID, ALICE, max_debt);

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				max_debt,
				Permill::from_percent(1)
			));

			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), max_debt);
		});
	}

	#[test]
	fn fails_insufficient_external_balance() {
		new_test_ext().execute_with(|| {
			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let psm_usdc_before = get_asset_balance(USDC_ASSET_ID, psm_account());
			let too_much = alice_usdc_before + 1000 * INTERNAL_UNIT;

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					too_much,
					Permill::from_percent(1)
				),
				TokenError::FundsUnavailable
			);

			// Verify no state mutation occurred
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), 0);
			assert_eq!(get_asset_balance(USDC_ASSET_ID, ALICE), alice_usdc_before);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), alice_internal_before);
			assert_eq!(get_asset_balance(USDC_ASSET_ID, psm_account()), psm_usdc_before);
		});
	}

	#[test]
	fn fails_mint_exceeds_aggregate_psm_ceiling() {
		new_test_ext().execute_with(|| {
			// Set both assets to 50% ratio each (100% total)
			// This tests that aggregate PSM ceiling is enforced even when per-asset ceilings allow
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(50));
			set_asset_ceiling_weight(USDT_ASSET_ID, Permill::from_percent(50));

			let max_psm_debt = crate::Pallet::<Test>::max_psm_debt(&INTERNAL_ASSET_ID);

			// Mint 50% of PSM ceiling via USDC (succeeds)
			let usdc_amount = Permill::from_percent(50).mul_floor(max_psm_debt);
			fund_external_asset(USDC_ASSET_ID, ALICE, usdc_amount);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				usdc_amount,
				Permill::from_percent(1)
			));

			// Try to mint 50% + 1 via USDT (total would exceed PSM ceiling)
			let usdt_amount = Permill::from_percent(50).mul_floor(max_psm_debt) + 1;
			fund_external_asset(USDT_ASSET_ID, BOB, usdt_amount);

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					USDT_ASSET_ID,
					usdt_amount,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}
}

mod redeem {
	use super::*;

	#[test]
	fn success_basic() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			let redeem_amount = 1000 * INTERNAL_UNIT;
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);
			let psm_usdc_before = get_asset_balance(USDC_ASSET_ID, psm_account());
			let debt_before = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				redeem_amount,
				Permill::from_percent(1)
			));

			let fee = Permill::from_percent(1).mul_ceil(redeem_amount);
			let external_to_user = redeem_amount - fee;

			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, ALICE),
				alice_internal_before - redeem_amount
			);
			assert_eq!(
				get_asset_balance(USDC_ASSET_ID, ALICE),
				alice_usdc_before + external_to_user
			);
			assert_eq!(
				get_asset_balance(USDC_ASSET_ID, psm_account()),
				psm_usdc_before - external_to_user
			);
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				debt_before - external_to_user
			);

			System::assert_has_event(
				Event::<Test>::Redeemed {
					who: ALICE,
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
					internal_consumed: redeem_amount,
					external_received: external_to_user,
					internal_fee: fee,
				}
				.into(),
			);
		});
	}

	#[test]
	fn fee_zero() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_redemption_fee(USDC_ASSET_ID, Permill::zero());

			let redeem_amount = 1000 * INTERNAL_UNIT;
			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				redeem_amount,
				Permill::zero()
			));

			assert_eq!(get_asset_balance(USDC_ASSET_ID, ALICE), alice_usdc_before + redeem_amount);
		});
	}

	#[test]
	fn fee_nonzero() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_redemption_fee(USDC_ASSET_ID, Permill::from_percent(5));

			let redeem_amount = 1000 * INTERNAL_UNIT;
			let fee = Permill::from_percent(5).mul_ceil(redeem_amount);
			let external_to_user = redeem_amount - fee;
			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				redeem_amount,
				Permill::from_percent(5)
			));

			assert_eq!(
				get_asset_balance(USDC_ASSET_ID, ALICE),
				alice_usdc_before + external_to_user
			);
		});
	}

	#[test]
	fn fee_100_percent() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_redemption_fee(USDC_ASSET_ID, Permill::from_percent(100));

			let redeem_amount = 1000 * INTERNAL_UNIT;
			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);
			let insurance_internal_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				redeem_amount,
				Permill::from_percent(100)
			));

			assert_eq!(get_asset_balance(USDC_ASSET_ID, ALICE), alice_usdc_before);
			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND),
				insurance_internal_before + redeem_amount
			);
		});
	}

	#[test]
	fn fails_when_fee_exceeds_max_fee() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_redemption_fee(USDC_ASSET_ID, Permill::from_percent(10));

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1),
				),
				Error::<Test>::FeeTooHigh
			);
		});
	}

	#[test]
	fn fails_unsupported_asset() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					UNSUPPORTED_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::zero()
				),
				Error::<Test>::UnsupportedAsset
			);
		});
	}

	#[test]
	fn fails_asset_all_disabled() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::AllDisabled);

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::AllSwapsStopped
			);
		});
	}

	#[test]
	fn allows_when_asset_minting_disabled() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::MintingDisabled);

			// Redemption should still work when only minting is disabled
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
		});
	}

	#[test]
	fn fails_below_minimum() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			let below_min = 50 * INTERNAL_UNIT;

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					below_min,
					Permill::from_percent(1)
				),
				Error::<Test>::BelowMinimumSwap
			);
		});
	}

	#[test]
	fn fails_insufficient_reserve() {
		new_test_ext().execute_with(|| {
			fund_internal(BOB, 10_000 * INTERNAL_UNIT);

			let reserve = get_asset_balance(USDC_ASSET_ID, psm_account());
			assert_eq!(reserve, 0);

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::InsufficientReserve
			);
		});
	}

	#[test]
	fn fails_insufficient_internal_balance() {
		ExtBuilder::default()
			.mints(ALICE, 5000 * INTERNAL_UNIT)
			.mints(BOB, 10_000 * INTERNAL_UNIT)
			.build_and_execute(|| {
				let alice_internal = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
				let too_much = alice_internal + 1000 * INTERNAL_UNIT;

				assert_noop!(
					Psm::redeem(
						RuntimeOrigin::signed(ALICE),
						INTERNAL_ASSET_ID,
						USDC_ASSET_ID,
						too_much,
						Permill::from_percent(1)
					),
					TokenError::FundsUnavailable
				);
			});
	}

	#[test]
	fn boundary_reserve_equals_output() {
		new_test_ext().execute_with(|| {
			set_minting_fee(USDC_ASSET_ID, Permill::zero());
			set_redemption_fee(USDC_ASSET_ID, Permill::zero());

			let amount = 5000 * INTERNAL_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				amount,
				Permill::zero()
			));
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				amount,
				Permill::zero()
			));

			assert_eq!(get_asset_balance(USDC_ASSET_ID, psm_account()), 0);
		});
	}

	#[test]
	fn fails_when_reserve_exceeds_debt_donated_reserves() {
		ExtBuilder::default().mints(ALICE, 5000 * INTERNAL_UNIT).build_and_execute(|| {
			set_redemption_fee(USDC_ASSET_ID, Permill::zero());

			let debt = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			let donation = 5000 * INTERNAL_UNIT;

			// Defensive path: simulate donated reserves by funding psm_account()
			// directly, bypassing mint to create a reserve > debt scenario.
			fund_external_asset(USDC_ASSET_ID, psm_account(), donation);

			let reserve = get_asset_balance(USDC_ASSET_ID, psm_account());
			assert!(reserve > debt, "reserve should exceed debt after donation");

			// Give user enough internal to try redeeming more than debt
			let redeem_amount = debt + donation;
			fund_internal(ALICE, redeem_amount);

			// Should fail because redemption is limited by debt, not reserve
			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					redeem_amount,
					Permill::zero()
				),
				Error::<Test>::InsufficientReserve
			);

			// Verify boundary: exactly debt works, but debt+1 does not
			hypothetically!({
				assert_ok!(Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					debt,
					Permill::zero()
				));
				assert_eq!(get_asset_balance(USDC_ASSET_ID, psm_account()), donation);
			});

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					debt + 1,
					Permill::zero()
				),
				Error::<Test>::InsufficientReserve
			);
		});
	}

	#[test]
	fn redeem_with_nonzero_fee_charges_fee_destination() {
		new_test_ext().execute_with(|| {
			set_redemption_fee(USDC_ASSET_ID, Permill::from_percent(1));
			let mint_fee = MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1_000 * INTERNAL_UNIT,
				mint_fee,
			));

			let held = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				held,
				Permill::from_percent(1)
			));
			assert!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND) > before);
		});
	}

	#[test]
	fn redeem_fails_when_sender_has_no_internal() {
		new_test_ext().execute_with(|| {
			use frame_support::traits::{
				fungibles::Mutate,
				tokens::{Fortitude, Precision, Preservation},
			};

			let mint_fee = MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1_000 * INTERNAL_UNIT,
				mint_fee,
			));
			set_redemption_fee(USDC_ASSET_ID, Permill::from_percent(1));

			let held = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			assert_ok!(Assets::burn_from(
				INTERNAL_ASSET_ID,
				&ALICE,
				held,
				Preservation::Expendable,
				Precision::BestEffort,
				Fortitude::Force,
			));

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1_000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				TokenError::FundsUnavailable
			);
		});
	}

	/// `burn_from` also burns a remainder below `min_balance`; the debt must drop by the same
	/// amount so that issuance keeps covering it.
	#[test]
	fn redeem_leaving_dust_keeps_issuance_equal_to_debt() {
		new_test_ext().execute_with(|| {
			let min_balance = 10_000;
			let fee = Permill::from_parts(100);
			pallet_assets::Asset::<Test>::mutate(INTERNAL_ASSET_ID, |details| {
				details.as_mut().expect("internal asset exists in the mock; qed").min_balance =
					min_balance
			});
			set_minting_fee(USDC_ASSET_ID, Permill::zero());
			set_redemption_fee(USDC_ASSET_ID, fee);
			crate::Psm::<Test>::mutate(INTERNAL_ASSET_ID, |info| {
				info.as_mut().expect("test PSM installed; qed").min_swap_amount = INTERNAL_UNIT
			});
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1_000 * INTERNAL_UNIT,
				Permill::zero(),
			));
			let dust = min_balance / 2;
			let redeem = get_asset_balance(INTERNAL_ASSET_ID, ALICE) - dust;

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				redeem,
				fee,
			));

			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), 0);
			assert_eq!(
				<Assets as frame_support::traits::fungibles::Inspect<AccountId>>::total_issuance(
					INTERNAL_ASSET_ID
				),
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID)
			);
			assert_ok!(crate::Pallet::<Test>::do_try_state());
		});
	}

	/// Dust minted against another external asset is sent to the fee destination instead of being
	/// burned, since the redeemed pair's debt cannot absorb it.
	#[test]
	fn redeem_sends_dust_from_other_pair_to_fee_destination() {
		new_test_ext().execute_with(|| {
			use frame_support::traits::{fungibles::Mutate, tokens::Preservation};

			let min_balance = 10_000;
			let dust = min_balance / 2;
			pallet_assets::Asset::<Test>::mutate(INTERNAL_ASSET_ID, |details| {
				details.as_mut().expect("internal asset exists in the mock; qed").min_balance =
					min_balance
			});
			set_minting_fee(USDC_ASSET_ID, Permill::zero());
			set_minting_fee(USDT_ASSET_ID, Permill::zero());
			set_redemption_fee(USDC_ASSET_ID, Permill::zero());

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1_000 * INTERNAL_UNIT,
				Permill::zero(),
			));
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				500 * INTERNAL_UNIT,
				Permill::zero(),
			));
			// The fee destination needs an account to receive an amount below `min_balance`.
			assert_ok!(<Assets as Mutate<AccountId>>::transfer(
				INTERNAL_ASSET_ID,
				&BOB,
				&INSURANCE_FUND,
				min_balance,
				Preservation::Preserve,
			));
			// Alice now holds dust backed by the USDT reserve.
			assert_ok!(<Assets as Mutate<AccountId>>::transfer(
				INTERNAL_ASSET_ID,
				&BOB,
				&ALICE,
				dust,
				Preservation::Preserve,
			));

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1_000 * INTERNAL_UNIT,
				Permill::zero(),
			));

			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), 0);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), min_balance + dust);
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), 0);
			assert_eq!(
				<Assets as frame_support::traits::fungibles::Inspect<AccountId>>::total_issuance(
					INTERNAL_ASSET_ID
				),
				crate::Pallet::<Test>::total_psm_debt(&INTERNAL_ASSET_ID)
			);
			assert_ok!(crate::Pallet::<Test>::do_try_state());
		});
	}
}

mod governance {
	use super::*;

	#[test]
	fn set_minting_fee_works() {
		new_test_ext().execute_with(|| {
			let old_fee = MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			let new_fee = Permill::from_percent(5);

			assert_ok!(Psm::set_minting_fee(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				new_fee
			));

			assert_eq!(MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), new_fee);

			System::assert_has_event(
				Event::<Test>::MintingFeeUpdated {
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
					old_value: old_fee,
					new_value: new_fee,
				}
				.into(),
			);
		});
	}

	#[test]
	fn set_minting_fee_unauthorized() {
		new_test_ext().execute_with(|| {
			let old_fee = MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_minting_fee(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(5)
				),
				DispatchError::BadOrigin
			);

			assert_eq!(MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), old_fee);
		});
	}

	#[test]
	fn set_redemption_fee_works() {
		new_test_ext().execute_with(|| {
			let old_fee = RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			let new_fee = Permill::from_percent(5);

			assert_ok!(Psm::set_redemption_fee(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				new_fee
			));

			assert_eq!(RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), new_fee);

			System::assert_has_event(
				Event::<Test>::RedemptionFeeUpdated {
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
					old_value: old_fee,
					new_value: new_fee,
				}
				.into(),
			);
		});
	}

	#[test]
	fn set_redemption_fee_unauthorized() {
		new_test_ext().execute_with(|| {
			let old_fee = RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_redemption_fee(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(5)
				),
				DispatchError::BadOrigin
			);

			assert_eq!(RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), old_fee);
		});
	}

	#[test]
	fn set_max_debt_works() {
		new_test_ext().execute_with(|| {
			let old_value = psm_max_debt();
			let new_value = 5_000_000 * INTERNAL_UNIT;

			assert_ok!(Psm::set_max_debt(RuntimeOrigin::root(), INTERNAL_ASSET_ID, new_value));

			assert_eq!(psm_max_debt(), new_value);

			System::assert_has_event(
				Event::<Test>::MaxDebtUpdated {
					internal_asset: INTERNAL_ASSET_ID,
					old_value,
					new_value,
				}
				.into(),
			);
		});
	}

	#[test]
	fn set_max_debt_unauthorized() {
		new_test_ext().execute_with(|| {
			let old_value = psm_max_debt();

			assert_noop!(
				Psm::set_max_debt(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					5_000_000 * INTERNAL_UNIT
				),
				DispatchError::BadOrigin
			);

			assert_eq!(psm_max_debt(), old_value);
		});
	}

	#[test]
	fn set_max_debt_below_asset_ceiling_pauses_minting_but_allows_redeem() {
		new_test_ext().execute_with(|| {
			// Test PSM splits ceilings USDC 60% / USDT 40%. Pin max_debt and fill USDC to its
			// 60% ceiling (600 of 1000).
			set_max_debt(1_000 * INTERNAL_UNIT);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				600 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// Lowering max_debt to 700 drops USDC's 60% ceiling to 420, below its 600 debt. This
			// is accepted: the ceiling is only a mint-time throttle, not a hard invariant.
			assert_ok!(Psm::set_max_debt(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				700 * INTERNAL_UNIT
			));

			// Minting USDC is paused while it sits above its ceiling...
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					100 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
			// ...but redemption still works and drains the debt back down.
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				200 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
			assert_ok!(Psm::do_try_state());
		});
	}

	#[test]
	fn set_asset_ceiling_weight_below_debt_pauses_minting_but_allows_redeem() {
		new_test_ext().execute_with(|| {
			set_max_debt(1_000 * INTERNAL_UNIT);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				600 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// USDC holds 600 at its 60% (=600) ceiling. Cutting its weight to 30% drops the
			// ceiling to (30/70)*1000 = 428, below the debt. This is accepted.
			assert_ok!(Psm::set_asset_ceiling_weight(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				Permill::from_percent(30)
			));

			// Minting USDC is paused while it sits above its ceiling...
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					100 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
			// ...redemption still drains it.
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				200 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
			assert_ok!(Psm::do_try_state());
		});
	}

	#[test]
	fn set_asset_status_works() {
		new_test_ext().execute_with(|| {
			let new_status = CircuitBreakerLevel::MintingDisabled;

			assert_ok!(Psm::set_asset_status(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				new_status
			));

			assert_eq!(
				ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID).map(|e| e.status),
				Some(new_status),
			);

			System::assert_has_event(
				Event::<Test>::AssetStatusUpdated {
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
					status: new_status,
				}
				.into(),
			);
		});
	}

	#[test]
	fn set_asset_status_unauthorized() {
		new_test_ext().execute_with(|| {
			let old_status = ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_asset_status(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					CircuitBreakerLevel::MintingDisabled
				),
				DispatchError::BadOrigin
			);

			assert_eq!(ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), old_status);
		});
	}

	#[test]
	fn set_asset_status_fails_unapproved_asset() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::set_asset_status(
					RuntimeOrigin::root(),
					INTERNAL_ASSET_ID,
					UNSUPPORTED_ASSET_ID,
					CircuitBreakerLevel::MintingDisabled
				),
				Error::<Test>::AssetNotApproved
			);
		});
	}

	#[test]
	fn set_asset_ceiling_weight_works() {
		new_test_ext().execute_with(|| {
			let old_ratio = AssetCeilingWeight::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			let new_ratio = Permill::from_percent(80);

			assert_ok!(Psm::set_asset_ceiling_weight(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				new_ratio
			));

			assert_eq!(
				AssetCeilingWeight::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				new_ratio
			);

			System::assert_has_event(
				Event::<Test>::AssetCeilingWeightUpdated {
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
					old_value: old_ratio,
					new_value: new_ratio,
				}
				.into(),
			);
		});
	}

	#[test]
	fn set_asset_ceiling_weight_unauthorized() {
		new_test_ext().execute_with(|| {
			let old_ratio = AssetCeilingWeight::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_asset_ceiling_weight(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(80)
				),
				DispatchError::BadOrigin
			);

			assert_eq!(
				AssetCeilingWeight::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				old_ratio
			);
		});
	}

	#[test]
	fn add_external_asset_works() {
		new_test_ext().execute_with(|| {
			let new_asset = 99u32;
			create_asset_with_metadata(new_asset);
			assert!(!Psm::is_approved_asset(&INTERNAL_ASSET_ID, &new_asset));

			assert_ok!(Psm::add_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				new_asset
			));

			assert!(Psm::is_approved_asset(&INTERNAL_ASSET_ID, &new_asset));

			System::assert_has_event(
				Event::<Test>::ExternalAssetAdded {
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: new_asset,
				}
				.into(),
			);
		});
	}

	#[test]
	fn add_external_asset_accepts_differing_decimals_within_range() {
		new_test_ext().execute_with(|| {
			let new_asset = 99u32;
			// Asset with 8 decimals vs internal's 6 — within MAX_DECIMALS_DIFF.
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), new_asset, ALICE, 1));
			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				new_asset,
				b"Eight Decimals".to_vec(),
				b"EIG".to_vec(),
				8
			));

			assert_ok!(Psm::add_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				new_asset
			));
			let stored = crate::ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, new_asset)
				.expect("external present");
			assert_eq!(stored.decimals, 8);
		});
	}

	#[test]
	fn add_external_asset_uses_recorded_internal_decimals_after_metadata_changes() {
		new_test_ext().execute_with(|| {
			let new_asset = 99u32;
			create_asset_with_metadata(new_asset);

			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				b"Internal Asset".to_vec(),
				b"INTERNAL".to_vec(),
				31
			));

			assert_ok!(Psm::add_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				new_asset
			));
			let stored = crate::ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, new_asset)
				.expect("external present");
			assert_eq!(stored.decimals, 6);
		});
	}

	#[test]
	fn add_external_asset_fails_decimals_out_of_range() {
		new_test_ext().execute_with(|| {
			let new_asset = 99u32;
			// Decimals 6 + 25 = 31 exceeds MAX_DECIMALS_DIFF (24).
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), new_asset, ALICE, 1));
			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				new_asset,
				b"Too Many Decimals".to_vec(),
				b"TMD".to_vec(),
				31
			));

			assert_noop!(
				Psm::add_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, new_asset),
				Error::<Test>::DecimalsRangeExceeded
			);
		});
	}

	#[test]
	fn add_external_asset_unauthorized() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::add_external_asset(RuntimeOrigin::signed(ALICE), INTERNAL_ASSET_ID, 99u32),
				DispatchError::BadOrigin
			);
		});
	}

	#[test]
	fn add_external_asset_fails_already_approved() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::add_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, USDC_ASSET_ID),
				Error::<Test>::AssetAlreadyApproved
			);
		});
	}

	#[test]
	fn add_external_asset_fails_when_asset_does_not_exist() {
		new_test_ext().execute_with(|| {
			// 12345 is not created in the fungibles pallet and not approved in PSM.
			let ghost: u32 = 12345;
			assert!(
				!<Assets as frame_support::traits::fungibles::Inspect<AccountId>>::asset_exists(
					ghost
				)
			);
			assert!(!crate::Pallet::<Test>::is_approved_asset(&INTERNAL_ASSET_ID, &ghost));

			assert_noop!(
				Psm::add_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, ghost),
				Error::<Test>::AssetDoesNotExist
			);
		});
	}

	#[test]
	fn add_external_asset_fails_too_many() {
		new_test_ext().execute_with(|| {
			use frame_support::traits::Get;
			let max: u32 = <Test as crate::Config>::MaxExternals::get();
			let existing =
				crate::ExternalAssets::<Test>::iter_prefix(INTERNAL_ASSET_ID).count() as u32;
			// Fill up to the limit.
			for i in 0..(max - existing) {
				let asset_id = 1000 + i;
				create_asset_with_metadata(asset_id);
				assert_ok!(Psm::add_external_asset(
					RuntimeOrigin::root(),
					INTERNAL_ASSET_ID,
					asset_id
				));
			}
			// One more should fail.
			create_asset_with_metadata(9999);
			assert_noop!(
				Psm::add_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, 9999),
				Error::<Test>::TooManyAssets
			);
		});
	}

	#[test]
	fn remove_external_asset_works() {
		new_test_ext().execute_with(|| {
			assert!(Psm::is_approved_asset(&INTERNAL_ASSET_ID, &USDC_ASSET_ID));

			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID
			));

			assert!(!Psm::is_approved_asset(&INTERNAL_ASSET_ID, &USDC_ASSET_ID));

			System::assert_has_event(
				Event::<Test>::ExternalAssetRemoved {
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDC_ASSET_ID,
				}
				.into(),
			);
		});
	}

	#[test]
	fn remove_external_asset_cleans_up_configuration() {
		new_test_ext().execute_with(|| {
			// Verify configuration exists before removal (explicitly set in genesis)
			assert!(MintingFee::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));
			assert!(RedemptionFee::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));
			assert!(AssetCeilingWeight::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));

			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID
			));

			// Verify storage entries are removed (not just set to default)
			assert!(!MintingFee::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));
			assert!(!RedemptionFee::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));
			assert!(!AssetCeilingWeight::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));
		});
	}

	#[test]
	fn remove_external_asset_unauthorized() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::remove_external_asset(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID
				),
				DispatchError::BadOrigin
			);
		});
	}

	#[test]
	fn remove_external_asset_fails_not_approved() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::remove_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, 99u32),
				Error::<Test>::AssetNotApproved
			);
		});
	}

	#[test]
	fn remove_external_asset_fails_has_debt() {
		ExtBuilder::default().mints(ALICE, 1000 * INTERNAL_UNIT).build_and_execute(|| {
			assert_noop!(
				Psm::remove_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, USDC_ASSET_ID),
				Error::<Test>::AssetHasDebt
			);
		});
	}

	#[test]
	fn remove_external_asset_succeeds_after_debt_drained() {
		new_test_ext().execute_with(|| {
			// Zero fees so a single mint/redeem pair brings debt exactly to 0.
			set_minting_fee(USDC_ASSET_ID, Permill::zero());
			set_redemption_fee(USDC_ASSET_ID, Permill::zero());

			// With non-zero debt, removal is blocked.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::zero()
			));
			assert_noop!(
				Psm::remove_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, USDC_ASSET_ID),
				Error::<Test>::AssetHasDebt
			);

			// Drain debt to zero — removal now succeeds.
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::zero()
			));
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), 0);
			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID
			));
			assert!(!ExternalAssets::<Test>::contains_key(INTERNAL_ASSET_ID, USDC_ASSET_ID));
		});
	}

	#[test]
	fn remove_external_asset_succeeds_after_debt_drained_with_external_decimal_drift() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_minting_fee(USDX_ASSET_ID, Permill::zero());
			set_redemption_fee(USDX_ASSET_ID, Permill::zero());

			let usdx_raw = 10_000 * USDX_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				usdx_raw,
				Permill::zero()
			));
			assert_noop!(
				Psm::remove_external_asset(RuntimeOrigin::root(), INTERNAL_ASSET_ID, USDX_ASSET_ID),
				Error::<Test>::AssetHasDebt
			);

			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				USDX_ASSET_ID,
				b"USDX".to_vec(),
				b"USDX".to_vec(),
				4
			));

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				10_000 * INTERNAL_UNIT,
				Permill::zero()
			));
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID), 0);
			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID
			));
			assert!(!ExternalAssets::<Test>::contains_key(INTERNAL_ASSET_ID, USDX_ASSET_ID));
		});
	}

	#[test]
	fn emergency_origin_can_set_asset_status() {
		new_test_ext().execute_with(|| {
			let new_status = CircuitBreakerLevel::MintingDisabled;

			assert_ok!(Psm::set_asset_status(
				RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				new_status
			));

			assert_eq!(
				ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID).map(|e| e.status),
				Some(new_status),
			);
		});
	}

	#[test]
	fn emergency_origin_cannot_set_minting_fee() {
		new_test_ext().execute_with(|| {
			let old_fee = MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_minting_fee(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(5)
				),
				Error::<Test>::InsufficientPrivilege
			);

			assert_eq!(MintingFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), old_fee);
		});
	}

	#[test]
	fn emergency_origin_cannot_set_redemption_fee() {
		new_test_ext().execute_with(|| {
			let old_fee = RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_redemption_fee(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(5)
				),
				Error::<Test>::InsufficientPrivilege
			);

			assert_eq!(RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID), old_fee);
		});
	}

	#[test]
	fn emergency_origin_cannot_set_max_debt() {
		new_test_ext().execute_with(|| {
			let old_value = psm_max_debt();

			assert_noop!(
				Psm::set_max_debt(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					old_value - INTERNAL_UNIT,
				),
				Error::<Test>::InsufficientPrivilege
			);

			assert_eq!(psm_max_debt(), old_value);
		});
	}

	#[test]
	fn emergency_origin_cannot_set_asset_ceiling_weight() {
		new_test_ext().execute_with(|| {
			let old_ratio = AssetCeilingWeight::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			assert_noop!(
				Psm::set_asset_ceiling_weight(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(30),
				),
				Error::<Test>::InsufficientPrivilege
			);

			assert_eq!(
				AssetCeilingWeight::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				old_ratio
			);
		});
	}

	#[test]
	fn emergency_origin_cannot_add_external_asset() {
		new_test_ext().execute_with(|| {
			let new_asset = 99u32;

			assert_noop!(
				Psm::add_external_asset(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					new_asset
				),
				Error::<Test>::InsufficientPrivilege
			);

			assert!(!Psm::is_approved_asset(&INTERNAL_ASSET_ID, &new_asset));
		});
	}

	#[test]
	fn emergency_origin_cannot_remove_external_asset() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::remove_external_asset(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID
				),
				Error::<Test>::InsufficientPrivilege
			);

			assert!(Psm::is_approved_asset(&INTERNAL_ASSET_ID, &USDC_ASSET_ID));
		});
	}

	#[test]
	fn set_minting_fee_fails_unapproved_asset() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::set_minting_fee(
					RuntimeOrigin::root(),
					INTERNAL_ASSET_ID,
					UNSUPPORTED_ASSET_ID,
					Permill::from_percent(5)
				),
				Error::<Test>::AssetNotApproved
			);
		});
	}

	#[test]
	fn set_redemption_fee_fails_unapproved_asset() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::set_redemption_fee(
					RuntimeOrigin::root(),
					INTERNAL_ASSET_ID,
					UNSUPPORTED_ASSET_ID,
					Permill::from_percent(5)
				),
				Error::<Test>::AssetNotApproved
			);
		});
	}

	#[test]
	fn set_asset_ceiling_weight_fails_unapproved_asset() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::set_asset_ceiling_weight(
					RuntimeOrigin::root(),
					INTERNAL_ASSET_ID,
					UNSUPPORTED_ASSET_ID,
					Permill::from_percent(50)
				),
				Error::<Test>::AssetNotApproved
			);
		});
	}
}

mod helpers {
	use super::*;

	#[test]
	fn max_psm_debt_reads_psm_info() {
		new_test_ext().execute_with(|| {
			set_max_debt(1_000_000 * INTERNAL_UNIT);

			assert_eq!(
				crate::Pallet::<Test>::max_psm_debt(&INTERNAL_ASSET_ID),
				1_000_000 * INTERNAL_UNIT
			);
		});
	}

	#[test]
	fn max_asset_debt_calculation() {
		new_test_ext().execute_with(|| {
			set_max_debt(1_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(60));

			let info = crate::Psm::<Test>::get(INTERNAL_ASSET_ID).unwrap();
			let max_asset_debt =
				crate::Pallet::<Test>::max_asset_debt(&INTERNAL_ASSET_ID, &USDC_ASSET_ID, &info);
			// 1M * 60% / (60% + 40%) = 600K
			let expected = sp_runtime::Perbill::from_rational(60u32, 100u32)
				.mul_floor(1_000_000 * INTERNAL_UNIT);

			assert_eq!(max_asset_debt, expected);
		});
	}

	#[test]
	fn is_approved_asset_true() {
		new_test_ext().execute_with(|| {
			assert!(crate::Pallet::<Test>::is_approved_asset(&INTERNAL_ASSET_ID, &USDC_ASSET_ID));
			assert!(crate::Pallet::<Test>::is_approved_asset(&INTERNAL_ASSET_ID, &USDT_ASSET_ID));
		});
	}

	#[test]
	fn is_approved_asset_false() {
		new_test_ext().execute_with(|| {
			assert!(!crate::Pallet::<Test>::is_approved_asset(
				&INTERNAL_ASSET_ID,
				&UNSUPPORTED_ASSET_ID
			));
			assert!(!crate::Pallet::<Test>::is_approved_asset(
				&INTERNAL_ASSET_ID,
				&INTERNAL_ASSET_ID
			));
		});
	}

	#[test]
	fn is_approved_asset_false_after_removal() {
		new_test_ext().execute_with(|| {
			// USDC is approved at genesis.
			assert!(crate::Pallet::<Test>::is_approved_asset(&INTERNAL_ASSET_ID, &USDC_ASSET_ID));

			// Removal flips the predicate.
			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID
			));
			assert!(!crate::Pallet::<Test>::is_approved_asset(&INTERNAL_ASSET_ID, &USDC_ASSET_ID));
		});
	}

	#[test]
	fn get_reserve_returns_balance() {
		new_test_ext().execute_with(|| {
			assert_eq!(crate::Pallet::<Test>::get_reserve(&INTERNAL_ASSET_ID, &USDC_ASSET_ID), 0);

			let mint_amount = 1000 * INTERNAL_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				mint_amount,
				Permill::from_percent(1)
			));

			assert_eq!(
				crate::Pallet::<Test>::get_reserve(&INTERNAL_ASSET_ID, &USDC_ASSET_ID),
				mint_amount
			);
		});
	}

	#[test]
	fn account_id_is_derived() {
		new_test_ext().execute_with(|| {
			let account = crate::Pallet::<Test>::psm_account(&INTERNAL_ASSET_ID);
			let entropy = (
				<frame_support::PalletId as TypeId>::TYPE_ID,
				<Test as crate::Config>::PalletId::get(),
				&INTERNAL_ASSET_ID,
			)
				.using_encoded(sp_io::hashing::blake2_256);
			let expected = AccountId::decode(&mut TrailingZeroInput::new(entropy.as_ref()))
				.expect("valid account");
			assert_eq!(account, expected);
			assert_ne!(account, ALICE);
			assert_ne!(account, BOB);
			assert_ne!(account, INSURANCE_FUND);
		});
	}
}

mod circuit_breaker {
	use super::*;

	#[test]
	fn circuit_breaker_full_transition_flow() {
		new_test_ext().execute_with(|| {
			// Zero fees so every mint/redeem amount maps 1:1 onto debt.
			set_minting_fee(USDC_ASSET_ID, Permill::zero());
			set_redemption_fee(USDC_ASSET_ID, Permill::zero());

			let asset = USDC_ASSET_ID;
			let amount = 100 * INTERNAL_UNIT;

			// Seed debt upfront so every redeem below has something to drain
			// against — the circuit breaker check is what we want to exercise,
			// not the debt floor.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				asset,
				500 * INTERNAL_UNIT,
				Permill::zero()
			));

			// Baseline: AllEnabled — both swaps work.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				asset,
				amount,
				Permill::zero()
			));
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				asset,
				amount,
				Permill::zero()
			));

			// Transition: AllEnabled -> MintingDisabled. Mint blocked, redeem
			// still works (useful for draining debt during a partial outage).
			assert_ok!(Psm::set_asset_status(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				asset,
				CircuitBreakerLevel::MintingDisabled,
			));
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					asset,
					amount,
					Permill::zero()
				),
				Error::<Test>::MintingStopped
			);
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				asset,
				amount,
				Permill::zero()
			));

			// Transition: MintingDisabled -> AllDisabled. Both blocked. Debt is
			// still > 0 here, so a redeem rejection is a real circuit-breaker
			// rejection (AllSwapsStopped), not an InsufficientReserve one.
			assert_ok!(Psm::set_asset_status(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				asset,
				CircuitBreakerLevel::AllDisabled,
			));
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					asset,
					amount,
					Permill::zero()
				),
				Error::<Test>::MintingStopped
			);
			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					asset,
					amount,
					Permill::zero()
				),
				Error::<Test>::AllSwapsStopped
			);

			// Transition: AllDisabled -> AllEnabled. Both resume normally.
			assert_ok!(Psm::set_asset_status(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				asset,
				CircuitBreakerLevel::AllEnabled,
			));
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				asset,
				amount,
				Permill::zero()
			));
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				asset,
				amount,
				Permill::zero()
			));
		});
	}
}

mod ceiling_redistribution {
	use super::*;

	#[test]
	fn zero_weight_redistributes_ceiling_to_others() {
		new_test_ext().execute_with(|| {
			// Setup: USDC 60%, USDT 40% of PSM ceiling
			// PSM ceiling = 50% of 20M = 10M
			// USDC ceiling = 60% of 10M = 6M
			// USDT ceiling = 40% of 10M = 4M
			set_max_debt(10_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(60));
			set_asset_ceiling_weight(USDT_ASSET_ID, Permill::from_percent(40));

			let max_psm = crate::Pallet::<Test>::max_psm_debt(&INTERNAL_ASSET_ID);
			assert_eq!(max_psm, 10_000_000 * INTERNAL_UNIT);

			// Normal ceiling for USDT = 4M
			let usdt_normal_ceiling = psm_max_asset_debt(USDT_ASSET_ID);
			assert_eq!(usdt_normal_ceiling, 4_000_000 * INTERNAL_UNIT);

			// Disable USDC minting and set weight to 0% (governance workflow)
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::MintingDisabled);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(0));

			// Now USDT should be able to use the full PSM ceiling
			// total_weight_sum = 0% + 40% = 40%
			// effective_weight = 40% / 40% = 100%
			// effective_ceiling = 100% of 10M = 10M
			fund_external_asset(USDT_ASSET_ID, BOB, 10_000_000 * INTERNAL_UNIT);

			// Mint up to the old ceiling (4M) - should work
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				4_000_000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// Mint another 5M - this would fail with old logic but should work now
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				5_000_000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// Total USDT debt should be 9M
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDT_ASSET_ID),
				9_000_000 * INTERNAL_UNIT
			);

			// Can't mint more than PSM ceiling (already at 9M, only 1M left)
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					USDT_ASSET_ID,
					2_000_000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}

	#[test]
	fn multiple_assets_share_redistributed_ceiling() {
		new_test_ext().execute_with(|| {
			// Add a third asset
			let bridged_usdc_asset_id = 4u32;
			create_asset_with_metadata(bridged_usdc_asset_id);
			assert_ok!(Psm::add_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				bridged_usdc_asset_id
			));

			// Setup: USDC 50%, USDT 25%, ETH:USDC 25%
			set_max_debt(10_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(50));
			set_asset_ceiling_weight(USDT_ASSET_ID, Permill::from_percent(25));
			set_asset_ceiling_weight(bridged_usdc_asset_id, Permill::from_percent(25));

			// PSM ceiling = 10M. USDC ceiling = 5M.
			// Mint 4M against USDC: creating real debt before lowering ceiling.
			fund_external_asset(USDC_ASSET_ID, ALICE, 4_000_000 * INTERNAL_UNIT);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				4_000_000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				4_000_000 * INTERNAL_UNIT
			);

			// Now disable USDC and set weight to 0%
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::MintingDisabled);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(0));

			// USDT and ETH:USDC now split the full ceiling
			// total_weight_sum = 0% + 25% + 25% = 50%
			// USDT effective_weight = 25% / 50% = 50% -> 5M ceiling
			// ETH:USDC effective_weight = 25% / 50% = 50% -> 5M ceiling

			fund_external_asset(USDT_ASSET_ID, ALICE, 6_000_000 * INTERNAL_UNIT);

			// USDT can mint up to 5M (redistributed ceiling)
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				5_000_000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// USDT can't mint more than its redistributed ceiling
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDT_ASSET_ID,
					1_000_000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}

	#[test]
	fn normal_weights_use_proportional_ceilings() {
		new_test_ext().execute_with(|| {
			// Setup: USDC 60%, USDT 40%
			set_max_debt(10_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(60));
			set_asset_ceiling_weight(USDT_ASSET_ID, Permill::from_percent(40));

			// Both assets have non-zero weights, should use proportional ceilings
			// USDT ceiling = 40% of 10M = 4M

			fund_external_asset(USDT_ASSET_ID, BOB, 5_000_000 * INTERNAL_UNIT);

			// Can mint up to 4M
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				4_000_000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// Can't mint more - exceeds per-asset ceiling
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					USDT_ASSET_ID,
					1_000_000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}

	#[test]
	fn single_asset_weight_always_normalizes_to_full_ceiling() {
		new_test_ext().execute_with(|| {
			// Remove USDT so only USDC remains
			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID
			));

			set_max_debt(10_000_000 * INTERNAL_UNIT);
			// PSM ceiling = 50% of 20M = 10M
			let mint_amount = 1000 * INTERNAL_UNIT;

			// Set USDC weight to 30% — with a single asset this normalizes to 100%
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(30));
			let ceiling_at_30 = psm_max_asset_debt(USDC_ASSET_ID);
			assert_eq!(ceiling_at_30, 10_000_000 * INTERNAL_UNIT);
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				mint_amount,
				Permill::from_percent(1)
			));

			// Change weight to 80% — still normalizes to 100%
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(80));
			let ceiling_at_80 = psm_max_asset_debt(USDC_ASSET_ID);
			assert_eq!(ceiling_at_80, 10_000_000 * INTERNAL_UNIT);

			// Setting weight to 0% disables minting
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(0));
			let ceiling_at_0 = psm_max_asset_debt(USDC_ASSET_ID);
			assert_eq!(ceiling_at_0, 0);
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					mint_amount,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}

	#[test]
	fn restoring_weight_restores_normal_ceilings() {
		new_test_ext().execute_with(|| {
			// Setup: USDC 60%, USDT 40%
			set_max_debt(10_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(60));
			set_asset_ceiling_weight(USDT_ASSET_ID, Permill::from_percent(40));

			fund_external_asset(USDT_ASSET_ID, BOB, 10_000_000 * INTERNAL_UNIT);

			// Disable USDC and set weight to 0% - USDT can use full ceiling
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::MintingDisabled);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(0));
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDT_ASSET_ID,
				5_000_000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));

			// Re-enable USDC and restore weight
			set_asset_status(USDC_ASSET_ID, CircuitBreakerLevel::AllEnabled);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(60));

			// Now USDT ceiling is back to 4M, but we already have 5M debt
			// Can't mint more
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					USDT_ASSET_ID,
					1_000_000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::ExceedsMaxPsmDebt
			);
		});
	}
}

mod cycles {
	use super::*;

	fn last_event() -> Event<Test> {
		System::events()
			.into_iter()
			.filter_map(|r| if let RuntimeEvent::Psm(inner) = r.event { Some(inner) } else { None })
			.next_back()
			.expect("Expected at least one PSM event")
	}

	#[test]
	fn mint_redeem_cycles_accounting() {
		new_test_ext().execute_with(|| {
			let cycles = 10u128;
			let amount = 1000 * INTERNAL_UNIT;

			// Ensure user has enough funds for all cycles
			fund_external_asset(USDC_ASSET_ID, ALICE, 1_000_000 * INTERNAL_UNIT);
			fund_external_asset(INTERNAL_ASSET_ID, ALICE, 1_000_000 * INTERNAL_UNIT);

			// Record initial balances
			let user_external_before = get_asset_balance(USDC_ASSET_ID, ALICE);
			let user_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let if_internal_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let psm_external_before = get_asset_balance(USDC_ASSET_ID, psm_account());

			let unit = INTERNAL_UNIT as f64;

			println!("=== Initial State ===");
			println!("User USDC: {:.2}", user_external_before as f64 / unit);
			println!("IF internal: {:.2}", if_internal_before as f64 / unit);
			println!("PSM USDC: {:.2}", psm_external_before as f64 / unit);
			println!(
				"PSM Debt: {:.2}",
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID) as f64 / unit
			);

			let mut total_mint_fees = 0u128;
			let mut total_redeem_fees = 0u128;

			for i in 0..cycles {
				// Mint
				assert_ok!(Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					amount,
					Permill::from_percent(1)
				));

				let (mint_fee, internal_received) = match last_event() {
					Event::Minted { internal_fee, internal_received, .. } => {
						(internal_fee, internal_received)
					},
					_ => panic!("Expected Minted event"),
				};
				total_mint_fees += mint_fee;

				println!(
					"\n=== Cycle {} - After Mint ({:.2} USDC) ===",
					i + 1,
					amount as f64 / unit
				);
				println!("Mint fee: {:.2}", mint_fee as f64 / unit);
				println!("internal received: {:.2}", internal_received as f64 / unit);
				println!("User USDC: {:.2}", get_asset_balance(USDC_ASSET_ID, ALICE) as f64 / unit);
				println!(
					"User internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, ALICE) as f64 / unit
				);
				println!(
					"PSM USDC: {:.2}",
					get_asset_balance(USDC_ASSET_ID, psm_account()) as f64 / unit
				);
				println!(
					"PSM Debt: {:.2}",
					PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID) as f64 / unit
				);
				println!(
					"IF internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND) as f64 / unit
				);

				// Redeem all internal received
				assert_ok!(Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					amount,
					Permill::from_percent(1)
				));

				let (redeem_fee, external_received) = match last_event() {
					Event::Redeemed { internal_fee, external_received, .. } => {
						(internal_fee, external_received)
					},
					_ => panic!("Expected Redeemed event"),
				};
				total_redeem_fees += redeem_fee;

				println!(
					"\n=== Cycle {} - After Redeem ({:.2} internal) ===",
					i + 1,
					amount as f64 / unit
				);
				println!("Redeem fee: {:.2}", redeem_fee as f64 / unit);
				println!("USDC received: {:.2}", external_received as f64 / unit);
				println!("User USDC: {:.2}", get_asset_balance(USDC_ASSET_ID, ALICE) as f64 / unit);
				println!(
					"User internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, ALICE) as f64 / unit
				);
				println!(
					"PSM USDC: {:.2}",
					get_asset_balance(USDC_ASSET_ID, psm_account()) as f64 / unit
				);
				println!(
					"PSM Debt: {:.2}",
					PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID) as f64 / unit
				);
				println!(
					"IF internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND) as f64 / unit
				);
			}

			// Final balances
			let user_external_after = get_asset_balance(USDC_ASSET_ID, ALICE);
			let user_internal_after = get_asset_balance(INTERNAL_ASSET_ID, ALICE);

			let if_internal_after = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let psm_external_after = get_asset_balance(USDC_ASSET_ID, psm_account());
			let psm_debt_after = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			println!("\n=== Final State ===");
			println!("User USDC: {:.2}", user_external_after as f64 / unit);
			println!(
				"User internal: {:.2}",
				get_asset_balance(INTERNAL_ASSET_ID, ALICE) as f64 / unit
			);
			println!("IF internal: {:.2}", if_internal_after as f64 / unit);
			println!("PSM USDC: {:.2}", psm_external_after as f64 / unit);
			println!("PSM Debt: {:.2}", psm_debt_after as f64 / unit);
			println!("Total mint fees: {:.2}", total_mint_fees as f64 / unit);
			println!("Total redeem fees: {:.2}", total_redeem_fees as f64 / unit);

			let total_fees = total_mint_fees + total_redeem_fees;
			let if_increase = if_internal_after - if_internal_before;
			let user_decrease = user_external_before - user_external_after + user_internal_before -
				user_internal_after;

			println!("\n=== Verification ===");
			println!("Total fees collected: {:.2}", total_fees as f64 / unit);
			println!("IF increase: {:.2}", if_increase as f64 / unit);
			println!("User decrease: {:.2}", user_decrease as f64 / unit);

			// Assertions
			// 1. IF balance increased by total fees (mint fees + redeem fees)
			assert_eq!(if_increase, total_fees, "IF should receive all fees");

			// 2. PSM external balance equals what remained after redemptions
			assert_eq!(psm_external_after, psm_debt_after, "PSM external = PSM debt");

			// 3. User external decrease equals total fees paid
			assert_eq!(user_decrease, total_fees, "User loss equals fees");

			// 4. PSM debt equals PSM external asset balance
			assert_eq!(
				psm_debt_after,
				get_asset_balance(USDC_ASSET_ID, psm_account()),
				"PSM debt equals PSM external balance"
			);
		});
	}

	#[test]
	fn infinite_until_debt_ceiling() {
		new_test_ext().execute_with(|| {
			let amount = 100_000 * INTERNAL_UNIT;

			// Set ceiling for ~1000 cycles
			// Each cycle: mint 100000, redeem 100000 → debt grows by ~1000 per cycle
			// For 1000 cycles: need ceiling > 110 + 1000 * 2.19 ≈ 2300
			// 2M * 50% (only USDC weighted) = 1M units ceiling
			set_max_debt(2_000_000 * INTERNAL_UNIT);
			set_asset_ceiling_weight(USDC_ASSET_ID, Permill::from_percent(50));

			let info = crate::Psm::<Test>::get(INTERNAL_ASSET_ID).unwrap();
			let max_debt =
				crate::Pallet::<Test>::max_asset_debt(&INTERNAL_ASSET_ID, &USDC_ASSET_ID, &info);

			println!("MAX DEBT: {}", max_debt);

			// Fund user with more than enough to hit the ceiling
			let funding = max_debt * 2;
			fund_external_asset(USDC_ASSET_ID, ALICE, funding);
			fund_external_asset(INTERNAL_ASSET_ID, ALICE, funding);
			// Record initial balances
			let user_external_before = get_asset_balance(USDC_ASSET_ID, ALICE);
			let user_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let if_internal_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let psm_external_before = get_asset_balance(USDC_ASSET_ID, psm_account());

			let unit = INTERNAL_UNIT as f64;

			println!("=== Initial State ===");
			println!("Max debt ceiling: {:.2}", max_debt as f64 / unit);
			println!("User USDC: {:.2}", user_external_before as f64 / unit);
			println!("User internal: {:.2}", user_internal_before as f64 / unit);
			println!("IF internal: {:.2}", if_internal_before as f64 / unit);
			println!("PSM USDC: {:.2}", psm_external_before as f64 / unit);
			println!(
				"PSM Debt: {:.2}",
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID) as f64 / unit
			);

			let mut total_mint_fees = 0u128;
			let mut total_redeem_fees = 0u128;
			let mut cycle = 0u128;

			loop {
				let current_debt = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

				// Check if we can mint another `amount`
				if current_debt + amount > max_debt {
					println!("\n=== Debt ceiling reached after {} cycles ===", cycle);
					println!("Current debt: {:.2}", current_debt as f64 / unit);
					println!("Max debt: {:.2}", max_debt as f64 / unit);
					println!("Cannot mint {:.2} more (would exceed ceiling)", amount as f64 / unit);
					break;
				}

				cycle += 1;

				// Mint
				assert_ok!(Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					amount,
					Permill::from_percent(1)
				));

				let (mint_fee, internal_received) = match last_event() {
					Event::Minted { internal_fee, internal_received, .. } => {
						(internal_fee, internal_received)
					},
					_ => panic!("Expected Minted event"),
				};
				total_mint_fees += mint_fee;

				println!(
					"\n=== Cycle {} - After Mint ({:.2} USDC) ===",
					cycle,
					amount as f64 / unit
				);
				println!("Mint fee: {:.2}", mint_fee as f64 / unit);
				println!("internal received: {:.2}", internal_received as f64 / unit);
				println!("User USDC: {:.2}", get_asset_balance(USDC_ASSET_ID, ALICE) as f64 / unit);
				println!(
					"User internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, ALICE) as f64 / unit
				);
				println!(
					"PSM USDC: {:.2}",
					get_asset_balance(USDC_ASSET_ID, psm_account()) as f64 / unit
				);
				println!(
					"PSM Debt: {:.2}",
					PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID) as f64 / unit
				);
				println!(
					"IF internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND) as f64 / unit
				);

				// Redeem all internal received
				assert_ok!(Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					amount,
					Permill::from_percent(1)
				));

				let (redeem_fee, external_received) = match last_event() {
					Event::Redeemed { internal_fee, external_received, .. } => {
						(internal_fee, external_received)
					},
					_ => panic!("Expected Redeemed event"),
				};
				total_redeem_fees += redeem_fee;

				println!(
					"\n=== Cycle {} - After Redeem ({:.2} internal) ===",
					cycle,
					amount as f64 / unit
				);
				println!("Redeem fee: {:.2}", redeem_fee as f64 / unit);
				println!("USDC received: {:.2}", external_received as f64 / unit);
				println!("User USDC: {:.2}", get_asset_balance(USDC_ASSET_ID, ALICE) as f64 / unit);
				println!(
					"User internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, ALICE) as f64 / unit
				);
				println!(
					"PSM USDC: {:.2}",
					get_asset_balance(USDC_ASSET_ID, psm_account()) as f64 / unit
				);
				println!(
					"PSM Debt: {:.2}",
					PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID) as f64 / unit
				);
				println!(
					"IF internal: {:.2}",
					get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND) as f64 / unit
				);
			}

			// Final balances
			let user_external_after = get_asset_balance(USDC_ASSET_ID, ALICE);
			let user_internal_after = get_asset_balance(INTERNAL_ASSET_ID, ALICE);

			let if_internal_after = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let psm_external_after = get_asset_balance(USDC_ASSET_ID, psm_account());
			let psm_debt_after = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			println!("\n=== Final State ===");
			println!("Total cycles: {}", cycle);
			println!("User USDC: {:.2}", user_external_after as f64 / unit);
			println!("User internal: {:.2}", user_internal_after as f64 / unit);

			println!("IF internal: {:.2}", if_internal_after as f64 / unit);
			println!("PSM USDC: {:.2}", psm_external_after as f64 / unit);
			println!("PSM Debt: {:.2}", psm_debt_after as f64 / unit);
			println!("Total mint fees: {:.2}", total_mint_fees as f64 / unit);
			println!("Total redeem fees: {:.2}", total_redeem_fees as f64 / unit);

			let total_fees = total_mint_fees + total_redeem_fees;
			let if_increase = if_internal_after - if_internal_before;
			let user_decrease = user_external_before - user_external_after + user_internal_before -
				user_internal_after;

			println!("\n=== Verification ===");
			println!("Total fees collected: {:.2}", total_fees as f64 / unit);
			println!("IF increase: {:.2}", if_increase as f64 / unit);
			println!("User decrease: {:.2}", user_decrease as f64 / unit);

			// Assertions
			assert!(cycle > 0, "Should have completed at least one cycle");
			assert_eq!(if_increase, total_fees, "IF should receive all fees");
			assert_eq!(psm_external_after, psm_debt_after, "PSM external = PSM debt");
			assert_eq!(user_decrease, total_fees, "User loss equals fees");
			assert!(psm_debt_after <= max_debt, "PSM debt should not exceed ceiling");

			// Redeem to fully drain PSM debt to 0
			// When redeeming: external_received = internal_paid - fee = internal_paid * (1 -
			// fee_rate) So: internal_paid = external_received / (1 - fee_rate)
			let fee_rate = RedemptionFee::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			println!("Fee Rate: {:#?}", fee_rate);
			let complement_parts = 1_000_000u128 - fee_rate.deconstruct() as u128;
			println!("Complemenet Part: {:#?}", complement_parts);
			let internal_needed = (psm_debt_after * 1_000_000).div_ceil(complement_parts);
			println!("internal Needed: {:#?}", internal_needed);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				internal_needed,
				Permill::from_percent(1)
			));

			let (redeem_fee, _external_received) = match last_event() {
				Event::Redeemed { internal_fee, external_received, .. } => {
					(internal_fee, external_received)
				},
				_ => panic!("Expected Redeemed event"),
			};
			total_redeem_fees += redeem_fee;

			// Final balances (after the drain redemption)
			let user_external_after = get_asset_balance(USDC_ASSET_ID, ALICE);
			let user_internal_after = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let if_internal_after = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let psm_external_after = get_asset_balance(USDC_ASSET_ID, psm_account());
			let psm_debt_after = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);

			let total_fees = total_mint_fees + total_redeem_fees;
			let if_increase = if_internal_after - if_internal_before;
			let user_decrease = user_external_before - user_external_after + user_internal_before -
				user_internal_after;

			println!("\n=== Final State ===");
			println!("Total cycles: {}", cycle);
			println!("User USDC: {:.2}", user_external_after as f64 / unit);
			println!("User internal: {:.2}", user_internal_after as f64 / unit);

			println!("IF internal: {:.2}", if_internal_after as f64 / unit);
			println!("PSM USDC: {:.2}", psm_external_after as f64 / unit);
			println!("PSM Debt: {:.2}", psm_debt_after as f64 / unit);
			println!("Total mint fees: {:.2}", total_mint_fees as f64 / unit);
			println!("Total redeem fees: {:.2}", total_redeem_fees as f64 / unit);

			println!("\n=== Verification ===");
			println!("Total fees collected: {:.2}", total_fees as f64 / unit);
			println!("IF increase: {:.2}", if_increase as f64 / unit);
			println!("User decrease: {:.2}", user_decrease as f64 / unit);

			assert_eq!(psm_debt_after, 0, "PSM debt should be fully drained to 0");
			assert_eq!(psm_external_after, 0, "PSM USDC balance should be 0");
			assert_eq!(user_external_after, user_external_before, "User USDC should be unchanged");
			assert_eq!(
				user_internal_after + if_internal_after,
				user_internal_before + if_internal_before,
				"internal conservation: user + IF at end should equal initial balances"
			);
			assert_eq!(
				total_fees, if_increase,
				"Total fees (mint + redeem) should equal IF internal increase"
			);
		});
	}
}

/// Tests for normalization between internal and external assets with different decimal
/// precision. Uses `USDX` (2 decimals) and `DAI_MOCK` (18 decimals) against a
/// 6-decimal internal. Both helper assets are created in pallet-assets genesis but
/// registered with PSM via `register_external_asset_with_weight` inside each test.
mod decimal_scaling {
	use super::*;
	use crate::MAX_DECIMALS_DIFF;

	fn set_zero_fees(asset_id: u32) {
		set_minting_fee(asset_id, Permill::zero());
		set_redemption_fee(asset_id, Permill::zero());
	}

	// Conversion helpers

	#[test]
	fn external_to_internal_same_decimals_is_identity() {
		new_test_ext().execute_with(|| {
			assert_eq!(Psm::external_to_internal(1_000_000, 6, 6).unwrap(), 1_000_000);
		});
	}

	#[test]
	fn external_to_internal_scale_up_is_exact() {
		new_test_ext().execute_with(|| {
			// USDX (2) -> internal (6): multiply by 10^4.
			assert_eq!(Psm::external_to_internal(100, 2, 6).unwrap(), 1_000_000);
		});
	}

	#[test]
	fn external_to_internal_scale_down_truncates() {
		new_test_ext().execute_with(|| {
			// DAI (18) -> internal (6): divide by 10^12, floor.
			assert_eq!(
				Psm::external_to_internal(1_500_000_000_000_000_123, 18, 6).unwrap(),
				1_500_000
			);
		});
	}

	#[test]
	fn internal_to_external_round_trip_bounds() {
		new_test_ext().execute_with(|| {
			// For any amount, round-trip should shrink or preserve.
			for (ext_decimals, internal_decimals) in [(2u8, 6u8), (6, 6), (18, 6), (6, 18), (6, 2)]
			{
				for amount in [0u128, 1, 100, 1_234_567, 10u128.pow(18)] {
					let fwd =
						Psm::external_to_internal(amount, ext_decimals, internal_decimals).unwrap();
					let rtp =
						Psm::internal_to_external(fwd, ext_decimals, internal_decimals).unwrap();
					assert!(rtp <= amount, "round-trip grew: amount={} got {}", amount, rtp);
				}
			}
		});
	}

	#[test]
	fn conversion_overflow_surfaces_error() {
		new_test_ext().execute_with(|| {
			// 10^40 overflows u128 (max ~3.4e38).
			assert!(Psm::external_to_internal(1, 0, 40).is_err());
		});
	}

	// Mint with scale-up (fewer external decimals)

	#[test]
	fn mint_scale_up_usdx_exact_no_dust() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(USDX_ASSET_ID);

			// 200 USDX raw = 2_00 = internal equivalent = 200 * 10^4 = 2_000_000 = 2 internal.
			// Use 10_000 * USDX_UNIT = 1_000_000 raw USDX so internal equivalent is above
			// MinSwapAmount (100 * INTERNAL_UNIT = 10^8).
			let usdx_raw = 10_000 * USDX_UNIT; // 1_000_000 raw USDX
			let expected_internal = 10_000 * INTERNAL_UNIT; // 10_000 internal
			let alice_usdx_before = get_asset_balance(USDX_ASSET_ID, ALICE);

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				usdx_raw,
				Permill::zero()
			));

			// User spent exactly usdx_raw (no dust path on scale-up).
			assert_eq!(get_asset_balance(USDX_ASSET_ID, ALICE), alice_usdx_before - usdx_raw);
			assert_eq!(get_asset_balance(USDX_ASSET_ID, psm_account()), usdx_raw);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), expected_internal);
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID), expected_internal);
		});
	}

	// Mint with scale-down (more external decimals)

	#[test]
	fn mint_scale_down_dai_leaves_dust_with_user() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(DAI_MOCK_ASSET_ID);

			// 100 DAI + 123 wei. internal equivalent = 100 * 10^6 = 10^8 (= MinSwapAmount).
			let dai_raw = 100 * DAI_UNIT + 123;
			let effective_dai = 100 * DAI_UNIT; // truncated to round-trip boundary
			let expected_internal = 100 * INTERNAL_UNIT;
			let alice_before = get_asset_balance(DAI_MOCK_ASSET_ID, ALICE);

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				dai_raw,
				Permill::zero()
			));

			// Only effective amount left the user; dust (123 wei) stays.
			assert_eq!(get_asset_balance(DAI_MOCK_ASSET_ID, ALICE), alice_before - effective_dai);
			assert_eq!(get_asset_balance(DAI_MOCK_ASSET_ID, psm_account()), effective_dai);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), expected_internal);
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID),
				expected_internal
			);
		});
	}

	#[test]
	fn mint_scale_down_dai_with_fee_keeps_dust_charges_only_fee() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(100));
			set_minting_fee(DAI_MOCK_ASSET_ID, Permill::from_percent(1));

			// DAI factor is 10^12 (18 decimals vs 6 for internal).
			//   deposit            = 100 DAI + 123 wei  (raw external units)
			//   internal_equivalent    = deposit / 10^12    = 100 internal
			//     (the 123 wei truncates — stays in ALICE's wallet)
			//   effective_external = internal_equivalent * 10^12 = 100 DAI (exact)
			//   fee (1%)           = mul_ceil(1% * internal_equivalent) = 1 internal
			//   internal_to_user       = internal_equivalent - fee = 99 internal
			//   dust (external)    = deposit - effective_external = 123 wei
			let deposit = 100 * DAI_UNIT + 123;
			let internal_equivalent = 100 * INTERNAL_UNIT;
			let effective_external = 100 * DAI_UNIT;
			let fee = 1 * INTERNAL_UNIT;
			let internal_to_user = 99 * INTERNAL_UNIT;
			let dust = 123u128;
			// Sanity: the submitted external amount is fully accounted for.
			assert_eq!(deposit, effective_external + dust);
			assert_eq!(internal_equivalent, internal_to_user + fee);

			let alice_dai_before = get_asset_balance(DAI_MOCK_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let if_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let debt_before = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID);

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				deposit,
				Permill::from_percent(1)
			));

			// ALICE keeps exactly `dust` of the submitted DAI; only
			// `effective_external` left her wallet into the PSM reserve.
			assert_eq!(
				get_asset_balance(DAI_MOCK_ASSET_ID, ALICE),
				alice_dai_before - deposit + dust,
				"dust must remain with the caller"
			);
			assert_eq!(get_asset_balance(DAI_MOCK_ASSET_ID, psm_account()), effective_external);
			// Minted internal split: user receives `internal_to_user`, fee dest gets `fee`.
			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, ALICE),
				alice_internal_before + internal_to_user
			);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), if_before + fee);
			// Debt grows by the internal-equivalent of the backed deposit.
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID),
				debt_before + internal_equivalent
			);

			// Minted event carries the effective external amount (what actually
			// entered the reserve), not the raw submission.
			System::assert_has_event(
				Event::<Test>::Minted {
					who: ALICE,
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: DAI_MOCK_ASSET_ID,
					external_consumed: effective_external,
					internal_received: internal_to_user,
					internal_fee: fee,
				}
				.into(),
			);
		});
	}

	#[test]
	fn mint_rejects_when_internal_equivalent_is_zero() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(100));

			// 999 wei DAI -> internal = 999 / 10^12 = 0.
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					DAI_MOCK_ASSET_ID,
					999,
					Permill::zero()
				),
				Error::<Test>::AmountTooSmallAfterConversion
			);
		});
	}

	#[test]
	fn mint_min_swap_is_enforced_on_internal_side() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(100));

			// 50 DAI = 50 internal equivalent, below MinSwapAmount (100 internal).
			let below = 50 * DAI_UNIT;
			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					DAI_MOCK_ASSET_ID,
					below,
					Permill::zero()
				),
				Error::<Test>::BelowMinimumSwap
			);
		});
	}

	// Redeem with scale-up (more external decimals)

	#[test]
	fn redeem_scale_up_dai_exact_no_dust() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(DAI_MOCK_ASSET_ID);

			// First mint so PSM has reserve and debt.
			let internal_amount = 1000 * INTERNAL_UNIT;
			let dai_raw = 1000 * DAI_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				dai_raw,
				Permill::zero()
			));
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID), internal_amount);

			// Redeem 500 internal -> expect exactly 500 DAI back.
			let redeem = 500 * INTERNAL_UNIT;
			let alice_dai_before = get_asset_balance(DAI_MOCK_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				redeem,
				Permill::zero()
			));

			assert_eq!(
				get_asset_balance(DAI_MOCK_ASSET_ID, ALICE),
				alice_dai_before + 500 * DAI_UNIT
			);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), alice_internal_before - redeem);
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID), redeem);
		});
	}

	// Redeem with scale-down (fewer external decimals)

	#[test]
	fn redeem_scale_down_usdx_with_fee_keeps_dust_charges_only_fee() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_minting_fee(USDX_ASSET_ID, Permill::zero());

			// Seed reserve and ALICE's internal balance with a prior 0-fee mint.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				10_000 * USDX_UNIT,
				Permill::zero()
			));

			set_redemption_fee(USDX_ASSET_ID, Permill::from_percent(1));

			// Pick a redeem amount that produces both a non-zero fee AND post-fee
			// round-trip dust. USDX factor is 10^4.
			//   redeem       = 200 internal + 17 units
			//   fee (1%)     = mul_ceil(1% * redeem) = 2 internal + 1 unit
			//   internal_net     = redeem - fee         = 198 internal + 16 units
			//   external_out = internal_net / 10^4      = 198 USDX (19_800 raw)
			//   eff_internal_net = external_out * 10^4  = 198 internal (198_000_000 raw)
			//   dust         = internal_net - eff_internal_net = 16   ← stays with user
			let redeem = 200 * INTERNAL_UNIT + 17;
			let fee = 2 * INTERNAL_UNIT + 1;
			let eff_internal_net = 198 * INTERNAL_UNIT;
			let external_out = 198 * USDX_UNIT;
			let dust = 16u128;
			// Sanity: the submitted amount is fully accounted for.
			assert_eq!(redeem, eff_internal_net + fee + dust);

			let alice_usdx_before = get_asset_balance(USDX_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let if_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let debt_before = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				redeem,
				Permill::from_percent(1)
			));

			// User receives exactly `external_out` USDX.
			assert_eq!(get_asset_balance(USDX_ASSET_ID, ALICE), alice_usdx_before + external_out);
			// ALICE keeps exactly `dust` of the submitted amount — the rest
			// (eff_internal_net burned + fee transferred) left her wallet.
			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, ALICE),
				alice_internal_before - redeem + dust,
				"dust must remain with the caller"
			);
			// FeeDestination receives only the nominal fee, not fee + dust.
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), if_before + fee);
			// Debt reduces by exactly the round-tripped internal amount.
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID),
				debt_before - eff_internal_net
			);

			// Redeemed event matches the actual movements: `internal_paid` reflects
			// the internal actually charged (burn + fee) with round-trip dust excluded,
			// `external_received` is the round-tripped external amount, and `fee`
			// is the nominal configured fee.
			System::assert_has_event(
				Event::<Test>::Redeemed {
					who: ALICE,
					internal_asset: INTERNAL_ASSET_ID,
					external_asset: USDX_ASSET_ID,
					internal_consumed: eff_internal_net + fee,
					external_received: external_out,
					internal_fee: fee,
				}
				.into(),
			);
		});
	}

	#[test]
	fn redeem_scale_down_usdx_dust_stays_with_user() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(USDX_ASSET_ID);

			// Mint first so PSM has reserve. 10_000 USDX -> 10_000 internal debt.
			let usdx_raw = 10_000 * USDX_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				usdx_raw,
				Permill::zero()
			));

			// Redeem 100 internal + 1 unit of dust. USDX has 2 decimals, so internal -> USDX
			// divides by 10^4. 100_000_001 internal -> 10_000 USDX (= 100_000_000 internal
			// worth). The 1-unit dust remains in ALICE's wallet (symmetric with
			// mint's behavior of leaving dust with the caller).
			let redeem = 100 * INTERNAL_UNIT + 1;
			let expected_usdx_out = 100 * USDX_UNIT;
			let alice_usdx_before = get_asset_balance(USDX_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let if_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let debt_before = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				redeem,
				Permill::zero()
			));

			assert_eq!(
				get_asset_balance(USDX_ASSET_ID, ALICE),
				alice_usdx_before + expected_usdx_out
			);
			// Only the round-tripped net (100 internal) was burned from ALICE; fees are
			// zero here so the dust (1 unit) stays in her wallet.
			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, ALICE),
				alice_internal_before - 100 * INTERNAL_UNIT
			);
			// Fee destination receives nothing (fee rate is zero).
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), if_before);
			// Debt reduced by what actually left the reserve in internal terms.
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID),
				debt_before - 100 * INTERNAL_UNIT
			);
		});
	}

	#[test]
	fn redeem_succeeds_with_100_percent_fee() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(USDX_ASSET_ID);

			// Seed the PSM reserve and ALICE's internal balance with a prior mint so
			// the redeem below has something to operate on.
			let usdx_raw = 10_000 * USDX_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				usdx_raw,
				Permill::zero()
			));
			set_redemption_fee(USDX_ASSET_ID, Permill::from_percent(100));

			let alice_usdx_before = get_asset_balance(USDX_ASSET_ID, ALICE);
			let if_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);

			// With fee = 100%, `internal_net` is zero. `external_out = 0` is then
			// legitimate (no truncation bug), so the swap must succeed: no internal
			// is burned, no external asset is transferred, the entire redeem
			// amount moves to the fee destination.
			let redeem = 100 * INTERNAL_UNIT;
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				redeem,
				Permill::from_percent(100)
			));

			// User receives zero USDX (100% fee).
			assert_eq!(get_asset_balance(USDX_ASSET_ID, ALICE), alice_usdx_before);
			// Fee destination gets the full redeemed internal.
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND), if_before + redeem);
		});
	}

	#[test]
	fn redeem_rejects_when_external_out_truncates_to_zero() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(USDX_ASSET_ID);

			// Seed the PSM reserve and ALICE's internal balance with a prior mint.
			let usdx_raw = 10_000 * USDX_UNIT;
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				usdx_raw,
				Permill::zero()
			));

			// Configure an extreme redemption fee so `internal_net > 0` but falls
			// below one USDX raw unit (factor 10^4). With MinSwapAmount = 10^8
			// internal and a 99.9999% fee:
			//   fee      = mul_ceil(999_999 * 10^8 / 10^6) = 99_999_900
			//   internal_net = 10^8 - 99_999_900 = 100
			//   external = 100 / 10^4 = 0  ← genuine truncation, must reject.
			set_redemption_fee(USDX_ASSET_ID, Permill::from_parts(999_999));

			let redeem = 100 * INTERNAL_UNIT;
			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDX_ASSET_ID,
					redeem,
					Permill::from_parts(999_999)
				),
				Error::<Test>::AmountTooSmallAfterConversion
			);
		});
	}

	// Metadata changes after registration

	#[test]
	fn mint_uses_snapshot_when_asset_decimals_drift() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(USDX_ASSET_ID);

			// Owner (ALICE) unilaterally changes USDX decimals from 2 -> 4.
			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				USDX_ASSET_ID,
				b"USDX".to_vec(),
				b"USDX".to_vec(),
				4
			));

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				10_000 * USDX_UNIT,
				Permill::zero()
			));
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID),
				10_000 * INTERNAL_UNIT
			);
		});
	}

	#[test]
	fn redeem_uses_snapshot_when_asset_decimals_drift() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(USDX_ASSET_ID);

			// Mint first, then change decimals.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				10_000 * USDX_UNIT,
				Permill::zero()
			));
			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				USDX_ASSET_ID,
				b"USDX".to_vec(),
				b"USDX".to_vec(),
				4
			));

			let alice_usdx_before = get_asset_balance(USDX_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let debt_before = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID);

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				100 * INTERNAL_UNIT,
				Permill::zero()
			));

			assert_eq!(
				get_asset_balance(USDX_ASSET_ID, ALICE),
				alice_usdx_before + 100 * USDX_UNIT
			);
			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, ALICE),
				alice_internal_before - 100 * INTERNAL_UNIT
			);
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID),
				debt_before - 100 * INTERNAL_UNIT
			);
		});
	}

	#[test]
	fn mint_uses_snapshot_when_internal_decimals_drift() {
		new_test_ext().execute_with(|| {
			set_zero_fees(USDC_ASSET_ID);
			// internal starts at 6 decimals; InternalDecimals snapshot matches. The owner
			// (ALICE) changes the internal asset's live metadata to simulate drift.
			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				b"INTERNAL".to_vec(),
				b"INTERNAL".to_vec(),
				8
			));

			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::zero()
			));
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				1000 * INTERNAL_UNIT
			);
		});
	}

	#[test]
	fn redeem_uses_snapshot_when_internal_decimals_drift() {
		new_test_ext().execute_with(|| {
			// Seed ALICE's internal balance and PSM reserve with a prior mint, then
			// drift the internal asset's decimals.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::from_percent(1)
			));
			assert_ok!(Assets::set_metadata(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				b"INTERNAL".to_vec(),
				b"INTERNAL".to_vec(),
				8
			));

			let alice_usdc_before = get_asset_balance(USDC_ASSET_ID, ALICE);
			let alice_internal_before = get_asset_balance(INTERNAL_ASSET_ID, ALICE);
			let insurance_internal_before = get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND);
			let debt_before = PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
			let redeem = 100 * INTERNAL_UNIT;
			let fee = Permill::from_percent(1).mul_ceil(redeem);
			let external_out = redeem - fee;

			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				redeem,
				Permill::from_percent(1)
			));

			assert_eq!(get_asset_balance(USDC_ASSET_ID, ALICE), alice_usdc_before + external_out);
			assert_eq!(get_asset_balance(INTERNAL_ASSET_ID, ALICE), alice_internal_before - redeem);
			assert_eq!(
				get_asset_balance(INTERNAL_ASSET_ID, INSURANCE_FUND),
				insurance_internal_before + fee
			);
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID),
				debt_before - external_out
			);
		});
	}

	#[test]
	fn mint_fails_when_psm_not_installed() {
		new_test_ext().execute_with(|| {
			crate::Psm::<Test>::remove(INTERNAL_ASSET_ID);

			assert_noop!(
				Psm::mint(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					1000 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::PsmNotFound
			);
		});
	}

	#[test]
	fn redeem_fails_when_psm_not_installed() {
		new_test_ext().execute_with(|| {
			fund_internal(ALICE, 1000 * INTERNAL_UNIT);
			crate::Psm::<Test>::remove(INTERNAL_ASSET_ID);

			assert_noop!(
				Psm::redeem(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					100 * INTERNAL_UNIT,
					Permill::from_percent(1)
				),
				Error::<Test>::PsmNotFound
			);
		});
	}

	// Snapshot and bookkeeping

	#[test]
	fn asset_decimals_snapshot_recorded_on_add_and_cleaned_on_remove() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(100));
			let stored = ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID)
				.expect("external present after add");
			assert_eq!(stored.decimals, 2);

			assert_ok!(Psm::remove_external_asset(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID
			));
			assert_eq!(ExternalAssets::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID), None);
		});
	}

	#[test]
	fn max_decimals_diff_const_is_protective() {
		// Compile-time sanity: the chosen bound is wide but below the overflow point.
		// 10^24 fits comfortably in u128 (< 10^38), and leaves ~10^14 headroom on
		// balances. The const is documented; this asserts it has not been widened
		// beyond the safe range.
		assert!(MAX_DECIMALS_DIFF <= 30);
	}

	// Mixed-decimal aggregate bookkeeping

	#[test]
	fn aggregate_debt_accrues_in_internal_units_across_mixed_decimal_assets() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(50));
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(50));
			set_zero_fees(USDX_ASSET_ID);
			set_zero_fees(DAI_MOCK_ASSET_ID);

			// Mint 500 internal-equivalent via USDX, 1500 internal-equivalent via DAI.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDX_ASSET_ID,
				500 * USDX_UNIT,
				Permill::zero()
			));
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				1500 * DAI_UNIT,
				Permill::zero()
			));

			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID), 500 * INTERNAL_UNIT);
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID),
				1500 * INTERNAL_UNIT
			);
			assert_eq!(Psm::total_psm_debt(&INTERNAL_ASSET_ID), 2000 * INTERNAL_UNIT);

			// do_try_state asserts invariants; invoke manually.
			assert_ok!(Psm::do_try_state());
		});
	}

	#[test]
	fn mixed_decimal_mint_redeem_cycles_round_trip_to_zero_debt() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(USDX_ASSET_ID, Permill::from_percent(50));
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(50));
			set_zero_fees(USDX_ASSET_ID);
			set_zero_fees(DAI_MOCK_ASSET_ID);

			// A mixed sequence: mint USDX, mint DAI, partially redeem each, mint
			// again, then drain both. After each step, try_state must hold. After
			// all steps, every asset's debt is back to zero.
			let steps: &[(u32, bool, u128)] = &[
				// (asset_id, is_mint, raw_amount)
				(USDX_ASSET_ID, true, 500 * USDX_UNIT), // mint 500 internal
				(DAI_MOCK_ASSET_ID, true, 1500 * DAI_UNIT), // mint 1500 internal
				(USDX_ASSET_ID, false, 200 * INTERNAL_UNIT), // redeem 200 internal via USDX
				(DAI_MOCK_ASSET_ID, false, 500 * INTERNAL_UNIT), // redeem 500 internal via DAI
				(USDX_ASSET_ID, true, 300 * USDX_UNIT), // mint another 300 internal
				(USDX_ASSET_ID, false, 600 * INTERNAL_UNIT), // drain USDX debt (500 - 200 + 300)
				(DAI_MOCK_ASSET_ID, false, 1000 * INTERNAL_UNIT), // drain DAI debt (1500 - 500)
			];

			for &(asset_id, is_mint, amount) in steps {
				if is_mint {
					assert_ok!(Psm::mint(
						RuntimeOrigin::signed(ALICE),
						INTERNAL_ASSET_ID,
						asset_id,
						amount,
						Permill::zero()
					));
				} else {
					assert_ok!(Psm::redeem(
						RuntimeOrigin::signed(ALICE),
						INTERNAL_ASSET_ID,
						asset_id,
						amount,
						Permill::zero()
					));
				}
				// try_state must hold after every step.
				assert_ok!(Psm::do_try_state());
			}

			// After draining both, per-asset debt and aggregate are zero.
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, USDX_ASSET_ID), 0);
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID), 0);
			assert_eq!(Psm::total_psm_debt(&INTERNAL_ASSET_ID), 0);

			// Reserves are also empty (zero fees, so no dust was charged).
			assert_eq!(get_asset_balance(USDX_ASSET_ID, psm_account()), 0);
			assert_eq!(get_asset_balance(DAI_MOCK_ASSET_ID, psm_account()), 0);
		});
	}

	#[test]
	fn try_state_holds_with_donated_mixed_decimal_reserve() {
		new_test_ext().execute_with(|| {
			register_external_asset_with_weight(DAI_MOCK_ASSET_ID, Permill::from_percent(100));
			set_zero_fees(DAI_MOCK_ASSET_ID);

			// Mint so the PSM has tracked debt + matching DAI reserve.
			assert_ok!(Psm::mint(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				1000 * DAI_UNIT,
				Permill::zero()
			));
			assert_eq!(
				PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID),
				1000 * INTERNAL_UNIT
			);

			// Donate extra DAI straight to the PSM account. Reserve now exceeds
			// internal_to_external(debt). try_state check 2 uses the external-side
			// comparison and must still pass.
			let psm = psm_account();
			fund_external_asset(DAI_MOCK_ASSET_ID, psm.clone(), 7 * DAI_UNIT);
			assert_eq!(get_asset_balance(DAI_MOCK_ASSET_ID, psm.clone()), 1007 * DAI_UNIT);

			// Invariants hold under a donated scale-up reserve.
			assert_ok!(Psm::do_try_state());

			// A redeem that exhausts tracked debt drains only the debt-backed
			// share; the donated 7 DAI stays trapped in reserve, and try_state
			// continues to hold.
			assert_ok!(Psm::redeem(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				DAI_MOCK_ASSET_ID,
				1000 * INTERNAL_UNIT,
				Permill::zero()
			));
			assert_eq!(PsmDebt::<Test>::get(INTERNAL_ASSET_ID, DAI_MOCK_ASSET_ID), 0);
			assert_eq!(get_asset_balance(DAI_MOCK_ASSET_ID, psm), 7 * DAI_UNIT);
			assert_ok!(Psm::do_try_state());
		});
	}
}

/// Lifecycle (`create_psm`/`remove_psm`) and per-instance admin reassignment
/// (`set_full_admin`/`set_emergency_admin`).
mod admin {
	use super::*;

	/// A fresh internal asset id with no PSM installed by the mock.
	const NEW_INTERNAL: u32 = 50;
	/// Minimum swap amount used when creating PSMs in these tests.
	const DEFAULT_MIN_SWAP: u128 = 100 * INTERNAL_UNIT;

	fn root_origin() -> OriginCaller {
		frame_system::RawOrigin::<AccountId>::Root.into()
	}

	fn signed_origin(who: AccountId) -> OriginCaller {
		frame_system::RawOrigin::<AccountId>::Signed(who).into()
	}

	#[test]
	fn create_psm_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			let reserved_before = Balances::reserved_balance(&ALICE);

			assert_ok!(Psm::create_psm(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(signed_origin(ALICE)),
				Box::new(signed_origin(ALICE)),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));

			// Hot record.
			let info = crate::Psm::<Test>::get(NEW_INTERNAL).expect("PSM created");
			assert_eq!(info.fee_destination, INSURANCE_FUND);
			assert_eq!(info.max_debt, DEFAULT_MAX_DEBT);
			assert_eq!(info.min_swap_amount, DEFAULT_MIN_SWAP);
			assert_eq!(info.external_count, 0);

			// Admin record: the signer is both admins and paid the creation deposit (holds are
			// accounted within `reserved`).
			let admin = crate::PsmAdmin::<Test>::get(NEW_INTERNAL).expect("admin record");
			assert_eq!(admin.full_admin, signed_origin(ALICE));
			assert_eq!(admin.emergency_admin, signed_origin(ALICE));
			assert!(matches!(admin.deposit, Some((ALICE, _))));
			assert!(Balances::reserved_balance(&ALICE) > reserved_before);

			System::assert_has_event(
				Event::<Test>::PsmCreated {
					internal_asset: NEW_INTERNAL,
					full_admin: Box::new(signed_origin(ALICE)),
					emergency_admin: Box::new(signed_origin(ALICE)),
					fee_destination: INSURANCE_FUND,
					max_debt: DEFAULT_MAX_DEBT,
				}
				.into(),
			);

			// The signer is now the full admin and can manage the instance.
			assert_ok!(Psm::set_max_debt(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, 1));
		});
	}

	#[test]
	fn create_psm_fails_if_already_exists() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::create_psm(
					RuntimeOrigin::signed(ALICE),
					INTERNAL_ASSET_ID,
					Box::new(signed_origin(ALICE)),
					Box::new(signed_origin(ALICE)),
					INSURANCE_FUND,
					DEFAULT_MAX_DEBT,
					DEFAULT_MIN_SWAP,
				),
				Error::<Test>::PsmAlreadyExists
			);
		});
	}

	#[test]
	fn create_psm_fails_if_asset_missing() {
		new_test_ext().execute_with(|| {
			assert_noop!(
				Psm::create_psm(
					RuntimeOrigin::root(),
					4242u32,
					Box::new(signed_origin(ALICE)),
					Box::new(signed_origin(ALICE)),
					INSURANCE_FUND,
					DEFAULT_MAX_DEBT,
					DEFAULT_MIN_SWAP,
				),
				Error::<Test>::AssetDoesNotExist
			);
		});
	}

	#[test]
	fn root_can_create_psm_without_deposit() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			let reserved_before = Balances::reserved_balance(&ALICE);

			assert_ok!(Psm::create_psm(
				RuntimeOrigin::root(),
				NEW_INTERNAL,
				Box::new(root_origin()),
				Box::new(root_origin()),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));

			let admin = crate::PsmAdmin::<Test>::get(NEW_INTERNAL).expect("admin record");
			assert_eq!(admin.deposit, None);
			assert_eq!(Balances::reserved_balance(&ALICE), reserved_before);
			assert_ok!(Psm::remove_psm(RuntimeOrigin::root(), NEW_INTERNAL));
		});
	}

	#[test]
	fn create_psm_fails_zero_min_swap() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			assert_noop!(
				Psm::create_psm(
					RuntimeOrigin::signed(ALICE),
					NEW_INTERNAL,
					Box::new(signed_origin(ALICE)),
					Box::new(signed_origin(ALICE)),
					INSURANCE_FUND,
					DEFAULT_MAX_DEBT,
					0,
				),
				Error::<Test>::ZeroMinSwapAmount
			);
		});
	}

	#[test]
	fn ensure_asset_owner_admits_only_the_owner() {
		// Exercises the pallet-provided `EnsureAssetOwner` directly (the mock's `CreateOrigin`
		// is plain signed). Only the asset owner's signed origin passes.
		use frame_support::traits::EnsureOriginWithArg;
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			assert_eq!(
				crate::EnsureAssetOwner::<Test>::try_origin(
					RuntimeOrigin::signed(ALICE),
					&NEW_INTERNAL,
				)
				.ok(),
				Some(Some(ALICE)),
			);
			assert!(crate::EnsureAssetOwner::<Test>::try_origin(
				RuntimeOrigin::signed(BOB),
				&NEW_INTERNAL,
			)
			.is_err());
			assert!(crate::EnsureAssetOwner::<Test>::try_origin(
				RuntimeOrigin::root(),
				&NEW_INTERNAL,
			)
			.is_err());
		});
	}

	#[test]
	fn create_psm_fails_if_deposit_cannot_be_taken() {
		new_test_ext().execute_with(|| {
			// An account that owns the internal asset but has no native balance to cover the
			// creation deposit. The consideration must reject it.
			const POOR: AccountId = AccountId::new([7; 32]);
			assert_ok!(Assets::force_create(RuntimeOrigin::root(), NEW_INTERNAL, POOR, true, 1));
			assert_eq!(Balances::free_balance(POOR), 0);

			assert_noop!(
				Psm::create_psm(
					RuntimeOrigin::signed(POOR),
					NEW_INTERNAL,
					Box::new(signed_origin(POOR)),
					Box::new(signed_origin(POOR)),
					INSURANCE_FUND,
					DEFAULT_MAX_DEBT,
					DEFAULT_MIN_SWAP,
				),
				TokenError::FundsUnavailable
			);
			// Nothing was created.
			assert!(!crate::Psm::<Test>::contains_key(NEW_INTERNAL));
		});
	}

	#[test]
	fn create_psm_charges_deposit_for_instance_footprint() {
		use codec::MaxEncodedLen;

		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			let reserved_before = Balances::reserved_balance(&ALICE);

			assert_ok!(Psm::create_psm(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(signed_origin(ALICE)),
				Box::new(signed_origin(ALICE)),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));

			// Recompute the expected deposit from the stored types instead of calling
			// `psm_creation_footprint`, so this test fails if that helper breaks.
			let items = 2u128;
			let size = (2 * u32::max_encoded_len() +
				crate::PsmInfo::<Test>::max_encoded_len() +
				crate::PsmAdminInfo::<Test>::max_encoded_len()) as u128;
			let expected = PsmCreationDeposit::get() + PsmDepositSlope::get() * items * size;
			assert_eq!(Balances::reserved_balance(&ALICE) - reserved_before, expected);
		});
	}

	#[test]
	fn remove_psm_works_and_refunds_creator() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			let reserved_before = Balances::reserved_balance(&ALICE);
			assert_ok!(Psm::create_psm(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(signed_origin(ALICE)),
				Box::new(signed_origin(ALICE)),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));
			assert!(Balances::reserved_balance(&ALICE) > reserved_before);

			assert_ok!(Psm::remove_psm(RuntimeOrigin::signed(ALICE), NEW_INTERNAL));

			assert!(!crate::Psm::<Test>::contains_key(NEW_INTERNAL));
			assert!(!crate::PsmAdmin::<Test>::contains_key(NEW_INTERNAL));
			// Deposit returned to the creator.
			assert_eq!(Balances::reserved_balance(&ALICE), reserved_before);
			System::assert_has_event(
				Event::<Test>::PsmRemoved { internal_asset: NEW_INTERNAL }.into(),
			);
		});
	}

	#[test]
	fn create_psm_then_remove_releases_provider_refs() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			let psm = crate::Pallet::<Test>::psm_account(&NEW_INTERNAL);
			let psm_before = frame_system::Account::<Test>::get(&psm).providers;
			let fee_before = frame_system::Account::<Test>::get(&INSURANCE_FUND).providers;

			assert_ok!(Psm::create_psm(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(signed_origin(ALICE)),
				Box::new(signed_origin(ALICE)),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));
			// create_psm acquired one provider reference on each of the reserve account and the
			// fee destination.
			assert_eq!(frame_system::Account::<Test>::get(&psm).providers, psm_before + 1);
			assert_eq!(
				frame_system::Account::<Test>::get(&INSURANCE_FUND).providers,
				fee_before + 1
			);

			assert_ok!(Psm::remove_psm(RuntimeOrigin::signed(ALICE), NEW_INTERNAL));
			// remove_psm released both — net zero, no provider leak across the lifecycle.
			assert_eq!(frame_system::Account::<Test>::get(&psm).providers, psm_before);
			assert_eq!(frame_system::Account::<Test>::get(&INSURANCE_FUND).providers, fee_before);
		});
	}

	#[test]
	fn remove_psm_refunds_original_depositor_after_reassign() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			let reserved_before = Balances::reserved_balance(&ALICE);
			assert_ok!(Psm::create_psm(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(signed_origin(ALICE)),
				Box::new(signed_origin(ALICE)),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));

			// Hand off full control to Root, then let Root remove the PSM.
			assert_ok!(Psm::set_full_admin(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(root_origin()),
			));
			assert_ok!(Psm::remove_psm(RuntimeOrigin::root(), NEW_INTERNAL));

			// The deposit still returns to ALICE, the original depositor.
			assert_eq!(Balances::reserved_balance(&ALICE), reserved_before);
		});
	}

	#[test]
	fn remove_psm_fails_with_approved_externals() {
		new_test_ext().execute_with(|| {
			// The PSM pre-installed by the test harness has USDC & USDT approved.
			assert_noop!(
				Psm::remove_psm(RuntimeOrigin::root(), INTERNAL_ASSET_ID),
				Error::<Test>::PsmHasApprovedExternals
			);
		});
	}

	#[test]
	fn remove_psm_fails_with_debt() {
		new_test_ext().execute_with(|| {
			assert_ok!(Assets::create(RuntimeOrigin::signed(ALICE), NEW_INTERNAL, ALICE, 1));
			assert_ok!(Psm::create_psm(
				RuntimeOrigin::signed(ALICE),
				NEW_INTERNAL,
				Box::new(signed_origin(ALICE)),
				Box::new(signed_origin(ALICE)),
				INSURANCE_FUND,
				DEFAULT_MAX_DEBT,
				DEFAULT_MIN_SWAP,
			));
			// Inject debt while there are no approved externals so the debt check is reached.
			PsmDebt::<Test>::insert(NEW_INTERNAL, USDC_ASSET_ID, 1u128);
			assert_noop!(
				Psm::remove_psm(RuntimeOrigin::signed(ALICE), NEW_INTERNAL),
				Error::<Test>::PsmHasDebt
			);
		});
	}

	#[test]
	fn remove_psm_unauthorized() {
		new_test_ext().execute_with(|| {
			// Stranger: the pre-installed test PSM's full_admin is Root.
			assert_noop!(
				Psm::remove_psm(RuntimeOrigin::signed(ALICE), INTERNAL_ASSET_ID),
				DispatchError::BadOrigin
			);
			// Emergency admin lacks the Full level.
			assert_noop!(
				Psm::remove_psm(RuntimeOrigin::signed(EMERGENCY_ACCOUNT), INTERNAL_ASSET_ID),
				Error::<Test>::InsufficientPrivilege
			);
		});
	}

	#[test]
	fn set_full_admin_works() {
		new_test_ext().execute_with(|| {
			// The pre-installed test PSM's full_admin is Root; hand off to ALICE.
			assert_ok!(Psm::set_full_admin(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				Box::new(signed_origin(ALICE)),
			));

			assert_eq!(
				crate::PsmAdmin::<Test>::get(INTERNAL_ASSET_ID).unwrap().full_admin,
				signed_origin(ALICE)
			);
			System::assert_has_event(
				Event::<Test>::FullAdminChanged {
					internal_asset: INTERNAL_ASSET_ID,
					old_admin: Box::new(root_origin()),
					new_admin: Box::new(signed_origin(ALICE)),
				}
				.into(),
			);

			// The new full admin can manage; the old one no longer can.
			assert_ok!(Psm::set_minting_fee(
				RuntimeOrigin::signed(ALICE),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				Permill::from_percent(2),
			));
			assert_noop!(
				Psm::set_minting_fee(
					RuntimeOrigin::root(),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					Permill::from_percent(3),
				),
				DispatchError::BadOrigin
			);
		});
	}

	#[test]
	fn set_full_admin_requires_full_admin() {
		new_test_ext().execute_with(|| {
			// Emergency admin cannot reassign.
			assert_noop!(
				Psm::set_full_admin(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					Box::new(signed_origin(BOB)),
				),
				Error::<Test>::InsufficientPrivilege
			);
			// Stranger cannot reassign.
			assert_noop!(
				Psm::set_full_admin(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					Box::new(signed_origin(BOB)),
				),
				DispatchError::BadOrigin
			);
			assert_eq!(
				crate::PsmAdmin::<Test>::get(INTERNAL_ASSET_ID).unwrap().full_admin,
				root_origin()
			);
		});
	}

	#[test]
	fn set_emergency_admin_works() {
		new_test_ext().execute_with(|| {
			assert_ok!(Psm::set_emergency_admin(
				RuntimeOrigin::root(),
				INTERNAL_ASSET_ID,
				Box::new(signed_origin(BOB)),
			));

			assert_eq!(
				crate::PsmAdmin::<Test>::get(INTERNAL_ASSET_ID).unwrap().emergency_admin,
				signed_origin(BOB)
			);
			System::assert_has_event(
				Event::<Test>::EmergencyAdminChanged {
					internal_asset: INTERNAL_ASSET_ID,
					old_admin: Box::new(signed_origin(EMERGENCY_ACCOUNT)),
					new_admin: Box::new(signed_origin(BOB)),
				}
				.into(),
			);

			// The new emergency admin can act; the old one no longer can.
			assert_ok!(Psm::set_asset_status(
				RuntimeOrigin::signed(BOB),
				INTERNAL_ASSET_ID,
				USDC_ASSET_ID,
				CircuitBreakerLevel::MintingDisabled,
			));
			assert_noop!(
				Psm::set_asset_status(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					USDC_ASSET_ID,
					CircuitBreakerLevel::AllEnabled,
				),
				DispatchError::BadOrigin
			);
		});
	}

	#[test]
	fn set_emergency_admin_requires_full_admin() {
		new_test_ext().execute_with(|| {
			// The emergency admin cannot reassign itself.
			assert_noop!(
				Psm::set_emergency_admin(
					RuntimeOrigin::signed(EMERGENCY_ACCOUNT),
					INTERNAL_ASSET_ID,
					Box::new(signed_origin(BOB)),
				),
				Error::<Test>::InsufficientPrivilege
			);
			// Stranger cannot reassign.
			assert_noop!(
				Psm::set_emergency_admin(
					RuntimeOrigin::signed(BOB),
					INTERNAL_ASSET_ID,
					Box::new(signed_origin(BOB)),
				),
				DispatchError::BadOrigin
			);
			assert_eq!(
				crate::PsmAdmin::<Test>::get(INTERNAL_ASSET_ID).unwrap().emergency_admin,
				signed_origin(EMERGENCY_ACCOUNT)
			);
		});
	}
}
