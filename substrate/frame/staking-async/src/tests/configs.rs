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

use super::*;

#[test]
fn set_staking_configs_works() {
	ExtBuilder::default().build_and_execute(|| {
		// setting works
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Set(1_500),
			ConfigOp::Set(2_000),
			ConfigOp::Set(10),
			ConfigOp::Set(20),
			ConfigOp::Set(Percent::from_percent(75)),
			ConfigOp::Set(Zero::zero()),
			ConfigOp::Set(Percent::from_percent(95)),
			ConfigOp::Set(false),
			ConfigOp::Set(10)
		));
		assert_eq!(MinNominatorBond::<Test>::get(), 1_500);
		assert_eq!(MinValidatorBond::<Test>::get(), 2_000);
		assert_eq!(MaxNominatorsCount::<Test>::get(), Some(10));
		assert_eq!(MaxValidatorsCount::<Test>::get(), Some(20));
		assert_eq!(ChillThreshold::<Test>::get(), Some(Percent::from_percent(75)));
		assert_eq!(MinCommission::<Test>::get(), Perbill::from_percent(0));
		assert_eq!(MaxStakedRewards::<Test>::get(), Some(Percent::from_percent(95)));
		assert_eq!(AreNominatorsSlashable::<Test>::get(), false);
		assert_eq!(ChillInactiveThreshold::<Test>::get(), 10);

		// noop does nothing
		assert_storage_noop!(assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop
		)));

		// removing works
		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove
		));
		assert_eq!(MinNominatorBond::<Test>::get(), 0);
		assert_eq!(MinValidatorBond::<Test>::get(), 0);
		assert_eq!(MaxNominatorsCount::<Test>::get(), None);
		assert_eq!(MaxValidatorsCount::<Test>::get(), None);
		assert_eq!(ChillThreshold::<Test>::get(), None);
		assert_eq!(MinCommission::<Test>::get(), Perbill::from_percent(0));
		assert_eq!(MaxStakedRewards::<Test>::get(), None);
		// AreNominatorsSlashable defaults to true when removed
		assert_eq!(AreNominatorsSlashable::<Test>::get(), true);
		assert_eq!(ChillInactiveThreshold::<Test>::get(), HistoryDepth::get());
	});
}

#[test]
fn set_max_commission_works() {
	ExtBuilder::default().build_and_execute(|| {
		let admin = 1; // AdminOrigin (see mock)
		let non_admin = 2;

		// GIVEN: Default is 100%
		assert_eq!(MaxCommission::<Test>::get(), Perbill::one());

		// WHEN/THEN: Root and admin can set, non-admin cannot
		assert_ok!(Staking::set_max_commission(RuntimeOrigin::root(), Perbill::from_percent(50)));
		assert_eq!(MaxCommission::<Test>::get(), Perbill::from_percent(50));

		assert_ok!(Staking::set_max_commission(
			RuntimeOrigin::signed(admin),
			Perbill::from_percent(25),
		));
		assert_eq!(MaxCommission::<Test>::get(), Perbill::from_percent(25));

		assert_noop!(
			Staking::set_max_commission(
				RuntimeOrigin::signed(non_admin),
				Perbill::from_percent(10)
			),
			BadOrigin
		);
	});
}

#[test]
fn max_commission_rejects_validate_above_max() {
	ExtBuilder::default().build_and_execute(|| {
		let alice = 11; // validator

		// GIVEN: MaxCommission set to 10%
		MaxCommission::<Test>::set(Perbill::from_percent(10));

		// WHEN/THEN: Above max rejected, at or below accepted
		assert_noop!(
			Staking::validate(
				RuntimeOrigin::signed(alice),
				ValidatorPrefs { commission: Perbill::from_percent(11), blocked: false }
			),
			Error::<Test>::CommissionTooHigh
		);

		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(alice),
			ValidatorPrefs { commission: Perbill::from_percent(10), blocked: false }
		));

		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(alice),
			ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
		));
	});
}

#[test]
fn max_commission_min_commission_invariant() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN: MinCommission = 10%
		assert_ok!(Staking::set_min_commission(RuntimeOrigin::root(), Perbill::from_percent(10)));

		// WHEN/THEN: Cannot set max below min
		assert_noop!(
			Staking::set_max_commission(RuntimeOrigin::root(), Perbill::from_percent(5)),
			Error::<Test>::CommissionTooLow
		);

		// GIVEN: MaxCommission = 50%
		assert_ok!(Staking::set_max_commission(RuntimeOrigin::root(), Perbill::from_percent(50)));

		// WHEN/THEN: Cannot set min above max
		assert_noop!(
			Staking::set_min_commission(RuntimeOrigin::root(), Perbill::from_percent(51)),
			Error::<Test>::CommissionTooHigh
		);

		// Equal values are fine
		assert_ok!(Staking::set_min_commission(RuntimeOrigin::root(), Perbill::from_percent(50)));
	});
}

#[test]
fn force_apply_min_commission_also_caps_to_max() {
	let prefs = |c| ValidatorPrefs { commission: Perbill::from_percent(c), blocked: false };
	ExtBuilder::default().build_and_execute(|| {
		let alice = 31; // validator
		let bob = 21; // validator

		assert_ok!(Staking::validate(RuntimeOrigin::signed(alice), prefs(50)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(bob), prefs(20)));

		// GIVEN: Max commission set to 30%
		MaxCommission::<Test>::set(Perbill::from_percent(30));

		// WHEN/THEN: Alice (50%) is capped to 30%
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), alice));
		assert_eq!(Validators::<Test>::get(alice), prefs(30));

		// Bob (20%) is already within range — no change
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), bob));
		assert_eq!(Validators::<Test>::get(bob), prefs(20));
	});
}
