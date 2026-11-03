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
use crate::{session_rotation::Eras, RewardKind, RewardPot};
use frame_support::dispatch::{extract_actual_weight, GetDispatchInfo, WithPostDispatchInfo};
use sp_runtime::{bounded_btree_map, traits::Dispatchable};

#[test]
fn rewards_with_nominator_should_work() {
	ExtBuilder::default().nominate(true).session_per_era(3).build_and_execute(|| {
		let init_balance_11 = asset::total_balance::<T>(&11);
		let init_balance_21 = asset::total_balance::<T>(&21);
		let init_balance_101 = asset::total_balance::<T>(&101);

		// Set payees
		Payee::<T>::insert(11, RewardDestination::Account(11));
		Payee::<T>::insert(21, RewardDestination::Account(21));
		Payee::<T>::insert(101, RewardDestination::Account(101));

		Eras::<T>::reward_active_era(vec![(11, 50)]);
		Eras::<T>::reward_active_era(vec![(11, 50)]);
		// This is the second validator of the current elected set.
		Eras::<T>::reward_active_era(vec![(21, 50)]);

		// Compute total payout now for whole duration of the session.
		let validator_payout_0 = validator_payout_for(time_per_era());

		assert_eq_uvec!(Session::validators(), vec![11, 21]);

		assert_eq!(asset::total_balance::<T>(&11), init_balance_11);
		assert_eq!(asset::total_balance::<T>(&21), init_balance_21);
		assert_eq!(asset::total_balance::<T>(&101), init_balance_101);
		assert_eq!(
			ErasRewardPoints::<T>::get(active_era()),
			EraRewardPoints { total: 50 * 3, individual: bounded_btree_map![11 => 100, 21 => 50] }
		);
		let part_for_11 = Perbill::from_rational::<u32>(1000, 1250);
		let part_for_21 = Perbill::from_rational::<u32>(1000, 1250);
		let part_for_101_from_11 = Perbill::from_rational::<u32>(250, 1250);
		let part_for_101_from_21 = Perbill::from_rational::<u32>(250, 1250);

		Session::roll_until_active_era(2);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::SessionRotated { starting_session: 4, active_era: 1, planned_era: 2 },
				Event::PagedElectionProceeded { page: 0, result: Ok(2) },
				Event::SessionRotated { starting_session: 5, active_era: 1, planned_era: 2 },
				Event::EraPaid { era_index: 1, validator_payout: 7500, remainder: 0 },
				Event::SessionRotated { starting_session: 6, active_era: 2, planned_era: 2 }
			]
		);
		// make note of total issuance before payouts (rewards already minted at era finalization
		// above)
		let pre_issuance = asset::total_issuance::<T>();

		mock::make_all_reward_payment(1);
		assert_eq!(
			mock::staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Account(11), amount: 3999 },
				Event::Rewarded { stash: 101, dest: RewardDestination::Account(101), amount: 1000 },
				Event::PayoutStarted { era_index: 1, validator_stash: 21, page: 0, next: None },
				Event::Rewarded { stash: 21, dest: RewardDestination::Account(21), amount: 1999 },
				Event::Rewarded { stash: 101, dest: RewardDestination::Account(101), amount: 500 }
			]
		);

		// In non-minting mode, rewards are minted externally (e.g. pallet-dap) and
		// snapshotted into era pots. Payouts transfer from pot, so issuance is unchanged.
		let post_issuance = asset::total_issuance::<T>();
		assert_eq!(post_issuance, pre_issuance);

		assert_eq_error_rate!(
			asset::total_balance::<T>(&11),
			init_balance_11 + part_for_11 * validator_payout_0 * 2 / 3,
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<T>(&21),
			init_balance_21 + part_for_21 * validator_payout_0 * 1 / 3,
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<T>(&101),
			init_balance_101 +
				part_for_101_from_11 * validator_payout_0 * 2 / 3 +
				part_for_101_from_21 * validator_payout_0 * 1 / 3,
			2
		);

		assert_eq_uvec!(Session::validators(), vec![11, 21]);
		Eras::<T>::reward_active_era(vec![(11, 1)]);

		// Compute total payout now for whole duration as other parameter won't change
		let total_payout_1 = validator_payout_for(time_per_era());

		Session::roll_until_active_era(3);

		assert_eq!(
			mock::staking_events_since_last_call(),
			vec![
				Event::SessionRotated { starting_session: 7, active_era: 2, planned_era: 3 },
				Event::PagedElectionProceeded { page: 0, result: Ok(2) },
				Event::SessionRotated { starting_session: 8, active_era: 2, planned_era: 3 },
				Event::EraPaid { era_index: 2, validator_payout: 7500, remainder: 0 },
				Event::SessionRotated { starting_session: 9, active_era: 3, planned_era: 3 }
			]
		);

		// Capture issuance before payout (rewards should already be minted)
		let pre_payout_2 = asset::total_issuance::<T>();

		mock::make_all_reward_payment(2);
		// With DAP, payouts just transfer from pot (no minting), so issuance unchanged
		assert_eq!(asset::total_issuance::<T>(), pre_payout_2);

		assert_eq_error_rate!(
			asset::total_balance::<T>(&11),
			init_balance_11 + part_for_11 * (validator_payout_0 * 2 / 3 + total_payout_1),
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<T>(&21),
			init_balance_21 + part_for_21 * validator_payout_0 * 1 / 3,
			2,
		);
		assert_eq_error_rate!(
			asset::total_balance::<T>(&101),
			init_balance_101 +
				part_for_101_from_11 * (validator_payout_0 * 2 / 3 + total_payout_1) +
				part_for_101_from_21 * validator_payout_0 * 1 / 3,
			2
		);
	});
}

#[test]
fn rewards_no_nominator_should_work() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		assert_eq_uvec!(Session::validators(), vec![11, 21]);

		// with no backers
		assert_eq_uvec!(
			era_exposures(1),
			vec![
				(11, Exposure::<AccountId, Balance> { total: 1000, own: 1000, others: vec![] }),
				(21, Exposure::<AccountId, Balance> { total: 1000, own: 1000, others: vec![] })
			]
		);

		// give them some points
		reward_all_elected();

		// go to next active era
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		// payout era 1
		make_all_reward_payment(1);

		// payout works
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Staked, amount: 3750 },
				Event::PayoutStarted { era_index: 1, validator_stash: 21, page: 0, next: None },
				Event::Rewarded { stash: 21, dest: RewardDestination::Staked, amount: 3750 }
			]
		);
	});
}

#[test]
fn nominating_and_rewards_should_work() {
	ExtBuilder::default()
		.nominate(false)
		.set_status(41, StakerStatus::Validator)
		.build_and_execute(|| {
			// initial validators, note that 41 has more stake than 11
			assert_eq_uvec!(Session::validators(), vec![41, 21]);

			// bond two monitors, both favouring 11
			bond_nominator(1, 5000, vec![11, 41]);
			bond_virtual_nominator(3, 333, 5000, vec![11]);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::Bonded { stash: 1, amount: 5000 },
					Event::Bonded { stash: 3, amount: 5000 },
				]
			);

			// reward our two winning validators
			Eras::<T>::reward_active_era(vec![(41, 1)]);
			Eras::<T>::reward_active_era(vec![(21, 1)]);

			Session::roll_until_active_era(2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 4, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 5, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 7500, remainder: 0 },
					Event::SessionRotated { starting_session: 6, active_era: 2, planned_era: 2 }
				]
			);

			// 11 now has more votes
			assert_eq_uvec!(Session::validators(), vec![11, 41]);
			assert_eq!(ErasStakersPaged::<T>::iter_prefix_values((active_era(),)).count(), 2);
			assert_eq!(
				Staking::eras_stakers(active_era(), &11),
				Exposure {
					total: 7500,
					own: 1000,
					others: vec![
						IndividualExposure { who: 1, value: 1500 },
						IndividualExposure { who: 3, value: 5000 }
					]
				}
			);
			assert_eq!(
				Staking::eras_stakers(active_era(), &41),
				Exposure {
					total: 7500,
					own: 4000,
					others: vec![IndividualExposure { who: 1, value: 3500 }]
				}
			);

			// payout era 1, in which 21 and 41 were validators with no nominators.
			mock::make_all_reward_payment(1);
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::PayoutStarted { era_index: 1, validator_stash: 21, page: 0, next: None },
					Event::Rewarded { stash: 21, dest: RewardDestination::Staked, amount: 3750 },
					Event::PayoutStarted { era_index: 1, validator_stash: 41, page: 0, next: None },
					Event::Rewarded { stash: 41, dest: RewardDestination::Staked, amount: 3750 }
				]
			);

			reward_all_elected();
			Session::roll_until_active_era(3);
			// ignore session rotation events, we've seen them before.
			let _ = staking_events_since_last_call();

			// for era 2 we had a nominator too, who is rewarded.
			mock::make_all_reward_payment(2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::PayoutStarted { era_index: 2, validator_stash: 11, page: 0, next: None },
					Event::Rewarded { stash: 11, dest: RewardDestination::Staked, amount: 499 },
					Event::Rewarded { stash: 1, dest: RewardDestination::Stash, amount: 750 },
					Event::Rewarded {
						stash: 3,
						dest: RewardDestination::Account(333),
						amount: 2500
					},
					Event::PayoutStarted { era_index: 2, validator_stash: 41, page: 0, next: None },
					Event::Rewarded { stash: 41, dest: RewardDestination::Staked, amount: 1999 },
					Event::Rewarded { stash: 1, dest: RewardDestination::Stash, amount: 1751 }
				]
			);
		});
}

#[test]
fn reward_destination_staked() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// initial conditions
		assert!(Session::validators().contains(&11));
		assert_eq!(Staking::payee(11.into()), Some(RewardDestination::Staked));
		assert_eq!(asset::total_balance::<T>(&11), 1001);
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// reward era 1 and payout at era 2
		Eras::<T>::reward_active_era(vec![(11, 1)]);
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		mock::make_all_reward_payment(1);
		assert_eq!(ClaimedRewards::<T>::get(1, &11), vec![0]);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Staked, amount: 7500 }
			]
		);

		// ledger must have been increased
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 8500,
				active: 8500,
				unlocking: Default::default(),
			}
		);
		// balance also updated
		assert_eq!(asset::total_balance::<T>(&11), 1001 + 7500);
	});
}

#[test]
fn reward_to_stake_works() {
	ExtBuilder::default()
		.nominate(false)
		.set_status(31, StakerStatus::Idle)
		.set_status(41, StakerStatus::Idle)
		.set_stake(21, 2000)
		.try_state(false)
		.build_and_execute(|| {
			assert_eq!(ValidatorCount::<T>::get(), 2);
			// Confirm account 10 and 20 are validators
			assert!(<Validators<T>>::contains_key(&11) && <Validators<T>>::contains_key(&21));

			assert_eq!(Staking::eras_stakers(active_era(), &11).total, 1000);
			assert_eq!(Staking::eras_stakers(active_era(), &21).total, 2000);

			// Give the man some money.
			let _ = asset::set_stakeable_balance::<T>(&10, 1000);
			let _ = asset::set_stakeable_balance::<T>(&20, 1000);

			// Bypass logic and change current exposure
			Eras::<T>::upsert_exposure(0, &21, Exposure { total: 69, own: 69, others: vec![] });
			<Ledger<T>>::insert(
				&20,
				StakingLedgerInspect {
					stash: 21,
					total: 69,
					active: 69,
					unlocking: Default::default(),
				},
			);

			// Compute total payout now for whole duration as other parameter won't change
			let validator_payout_0 = validator_payout_for(time_per_era());
			Pallet::<T>::reward_by_ids(vec![(11, 1)]);
			Pallet::<T>::reward_by_ids(vec![(21, 1)]);

			// New era --> rewards are paid --> stakes are changed
			Session::roll_until_active_era(2);
			make_all_reward_payment(1);

			assert_eq!(Staking::eras_stakers(active_era(), &11).total, 1000);
			assert_eq!(Staking::eras_stakers(active_era(), &21).total, 2000);

			let _11_balance = asset::stakeable_balance::<T>(&11);
			assert_eq!(_11_balance, 1000 + validator_payout_0 / 2);

			// Trigger another new era as the info are frozen before the era start.
			Session::roll_until_active_era(3);

			// -- new infos
			assert_eq!(
				Staking::eras_stakers(active_era(), &11).total,
				1000 + validator_payout_0 / 2
			);
			assert_eq!(
				Staking::eras_stakers(active_era(), &21).total,
				2000 + validator_payout_0 / 2
			);
		});
}

#[test]
fn reward_destination_stash() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// initial conditions
		assert!(Session::validators().contains(&11));
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));
		assert_eq!(asset::total_balance::<T>(&11), 1001);
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// reward era 1 and payout at era 2
		Eras::<T>::reward_active_era(vec![(11, 1)]);
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		mock::make_all_reward_payment(1);
		assert_eq!(ClaimedRewards::<T>::get(1, &11), vec![0]);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Stash, amount: 7500 }
			]
		);

		// ledger same, balance increased
		assert_eq!(asset::total_balance::<T>(&11), 1001 + 7500);
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);
	});
}

#[test]
fn reward_destination_account() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// initial conditions
		assert!(Session::validators().contains(&11));
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Account(7)));

		assert_eq!(asset::total_balance::<T>(&11), 1001);
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// reward era 1 and payout at era 2
		Eras::<T>::reward_active_era(vec![(11, 1)]);
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		mock::make_all_reward_payment(1);
		assert_eq!(ClaimedRewards::<T>::get(1, &11), vec![0]);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Account(7), amount: 7500 }
			]
		);

		// balance and ledger the same, 7 is unded
		assert_eq!(asset::total_balance::<T>(&11), 1001);
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);
		assert_eq!(asset::total_balance::<T>(&7), 7500);
	});
}

#[test]
fn validator_prefs_no_commission() {
	ExtBuilder::default().build_and_execute(|| {
		Eras::<T>::reward_active_era(vec![(11, 1)]);

		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		mock::make_all_reward_payment(1);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Staked, amount: 6000 },
				Event::Rewarded { stash: 101, dest: RewardDestination::Staked, amount: 1500 }
			]
		);
	});
}

#[test]
fn validator_prefs_100_commission() {
	ExtBuilder::default().build_and_execute(|| {
		let commission = Perbill::from_percent(100);
		Eras::<T>::reward_active_era(vec![(11, 1)]);

		Eras::<T>::set_validator_prefs(1, &11, ValidatorPrefs { commission, ..Default::default() });
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		mock::make_all_reward_payment(1);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Staked, amount: 7500 }
			]
		);
	});
}

#[test]
fn validator_payment_some_commission_prefs_work() {
	ExtBuilder::default().build_and_execute(|| {
		let commission = Perbill::from_percent(40);
		Eras::<T>::reward_active_era(vec![(11, 1)]);

		Eras::<T>::set_validator_prefs(1, &11, ValidatorPrefs { commission, ..Default::default() });
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		mock::make_all_reward_payment(1);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Staked, amount: 6600 },
				Event::Rewarded { stash: 101, dest: RewardDestination::Staked, amount: 900 }
			]
		);
	});
}

#[test]
fn min_commission_works() {
	ExtBuilder::default().build_and_execute(|| {
		// account 11 controls the stash of itself.
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
		));

		// event emitted should be correct
		assert_eq!(
			*staking_events().last().unwrap(),
			Event::ValidatorPrefsSet {
				stash: 11,
				prefs: ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
			}
		);

		assert_ok!(Staking::set_staking_configs(
			RuntimeOrigin::root(),
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Set(Perbill::from_percent(10)),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop
		));

		// can't make it less than 10 now
		assert_noop!(
			Staking::validate(
				RuntimeOrigin::signed(11),
				ValidatorPrefs { commission: Perbill::from_percent(5), blocked: false }
			),
			Error::<T>::CommissionTooLow
		);

		// can only change to higher.
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(10), blocked: false }
		));

		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(15), blocked: false }
		));
	})
}

#[test]
fn set_min_commission_works_with_admin_origin() {
	ExtBuilder::default().build_and_execute(|| {
		// no minimum commission set initially
		assert_eq!(MinCommission::<T>::get(), Zero::zero());

		// root can set min commission
		assert_ok!(Staking::set_min_commission(RuntimeOrigin::root(), Perbill::from_percent(10)));

		assert_eq!(MinCommission::<T>::get(), Perbill::from_percent(10));

		// Non privileged origin can not set min_commission
		assert_noop!(
			Staking::set_min_commission(RuntimeOrigin::signed(2), Perbill::from_percent(15)),
			BadOrigin
		);

		// Admin Origin can set min commission
		assert_ok!(Staking::set_min_commission(
			RuntimeOrigin::signed(1),
			Perbill::from_percent(15),
		));

		// setting commission below min_commission fails
		assert_noop!(
			Staking::validate(
				RuntimeOrigin::signed(11),
				ValidatorPrefs { commission: Perbill::from_percent(14), blocked: false }
			),
			Error::<T>::CommissionTooLow
		);

		// setting commission >= min_commission works
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(15), blocked: false }
		));
	})
}

#[test]
fn force_apply_min_commission_works() {
	let prefs = |c| ValidatorPrefs { commission: Perbill::from_percent(c), blocked: false };
	let validators = || Validators::<T>::iter().collect::<Vec<_>>();
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Staking::validate(RuntimeOrigin::signed(31), prefs(10)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(21), prefs(5)));

		// Given
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(0))]);
		MinCommission::<T>::set(Perbill::from_percent(5));

		// When applying to a commission greater than min
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 31));
		// Then the commission is not changed
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(0))]);

		// When applying to a commission that is equal to min
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 21));
		// Then the commission is not changed
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(0))]);

		// When applying to a commission that is less than the min
		assert_ok!(Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 11));
		// Then the commission is bumped to the min
		assert_eq!(validators(), vec![(31, prefs(10)), (21, prefs(5)), (11, prefs(5))]);

		// When applying commission to a validator that doesn't exist then storage is not altered
		assert_noop!(
			Staking::force_apply_min_commission(RuntimeOrigin::signed(1), 420),
			Error::<T>::NotStash
		);
	});
}

#[test]
fn claim_reward_at_the_last_era_and_no_double_claim_and_invalid_claim() {
	// should check that:
	// * rewards get paid until history_depth for both validators and nominators
	// * an invalid era to claim doesn't update last_reward
	// * double claim of one era fails
	ExtBuilder::default().nominate(true).build_and_execute(|| {
		// Consumed weight for all payout_stakers dispatches that fail
		let err_weight = <T as Config>::WeightInfo::payout_stakers_alive_staked(0);

		// Check state
		Payee::<T>::insert(11, RewardDestination::Account(11));
		Payee::<T>::insert(101, RewardDestination::Account(101));

		// reward for era 1
		Pallet::<T>::reward_by_ids(vec![(11, 1)]);

		Session::roll_until_active_era(2);

		// reward for era 2
		Pallet::<T>::reward_by_ids(vec![(11, 1)]);

		Session::roll_until_active_era(3);

		// reward for era 3
		Pallet::<T>::reward_by_ids(vec![(11, 1)]);

		// go to the history depth era
		Session::roll_until_active_era(HistoryDepth::get() + 1);
		let _ = staking_events_since_last_call();

		// Last kept is 1:
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 0, 0),
			// Fail: Era out of history
			Error::<T>::InvalidEraToReward.with_weight(err_weight)
		);

		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 1, 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Account(11), amount: 6000 },
				Event::Rewarded { stash: 101, dest: RewardDestination::Account(101), amount: 1500 }
			]
		);

		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::PayoutStarted { era_index: 2, validator_stash: 11, page: 0, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Account(11), amount: 6000 },
				Event::Rewarded { stash: 101, dest: RewardDestination::Account(101), amount: 1500 }
			]
		);

		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0),
			// Fail: Double claim
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, active_era(), 0),
			// Fail: Era ongoing
			Error::<T>::InvalidEraToReward.with_weight(err_weight)
		);
	});
}

#[test]
fn nominators_over_max_exposure_page_size_are_rewarded() {
	ExtBuilder::default().build_and_execute(|| {
		// bond one nominator more than the max exposure page size to validator 11 in era 1
		for i in 0..=MaxExposurePageSize::get() {
			let stash = 10_000 + i as AccountId;
			let balance = 10_000 + i as Balance;
			asset::set_stakeable_balance::<T>(&stash, balance);
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(stash),
				balance,
				RewardDestination::Stash
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(stash), vec![11]));
		}

		// enact new staker set -- era 2
		Session::roll_until_active_era(2);

		// reward for era 2
		Pallet::<T>::reward_by_ids(vec![(11, 1)]);

		Session::roll_until_active_era(3);
		mock::make_all_reward_payment(2);

		// Assert nominators from 1 to Max are rewarded
		let mut i: u32 = 0;
		while i < MaxExposurePageSize::get() {
			let stash = 10_000 + i as AccountId;
			let balance = 10_000 + i as Balance;
			assert!(asset::stakeable_balance::<T>(&stash) > balance);
			i += 1;
		}

		// Assert overflowing nominators from page 1 are also rewarded
		let stash = 10_000 + i as AccountId;
		assert!(asset::stakeable_balance::<T>(&stash) > (10_000 + i) as Balance);
	});
}

#[test]
fn test_nominators_are_rewarded_for_all_exposure_page() {
	ExtBuilder::default().build_and_execute(|| {
		// 3 pages of exposure
		let nominator_count = 2 * MaxExposurePageSize::get() + 1;

		for i in 0..nominator_count {
			let stash = 10_000 + i as AccountId;
			let balance = 10_000 + i as Balance;
			asset::set_stakeable_balance::<T>(&stash, balance);
			assert_ok!(Staking::bond(
				RuntimeOrigin::signed(stash),
				balance,
				RewardDestination::Stash
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(stash), vec![11]));
		}

		// enact
		Session::roll_until_active_era(2);

		// give rewards
		Pallet::<T>::reward_by_ids(vec![(11, 1)]);

		Session::roll_until_active_era(3);
		mock::make_all_reward_payment(2);

		assert_eq!(Eras::<T>::exposure_page_count(2, &11), 3);

		// Assert all nominators are rewarded according to their stake
		for i in 0..nominator_count {
			// balance of the nominator after the reward payout.
			let current_balance = asset::stakeable_balance::<T>(&((10000 + i) as AccountId));
			// balance of the nominator in the previous iteration.
			let previous_balance = asset::stakeable_balance::<T>(&((10000 + i - 1) as AccountId));
			// balance before the reward.
			let original_balance = 10_000 + i as Balance;

			assert!(current_balance > original_balance);
			// since the stake of the nominator is increasing for each iteration, the final balance
			// after the reward should also be higher than the previous iteration.
			assert!(current_balance > previous_balance);
		}
	});
}

#[test]
fn test_multi_page_payout_stakers_by_page() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Track the exposure of the validator and all nominators.
		let mut total_exposure = balance;

		// Create a validator:
		bond_validator(11, balance); // Default(64)
		assert_eq!(Validators::<T>::count(), 1);

		// Create nominators, targeting stash of validators
		for i in 0..100 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
			// with multi page reward payout, payout exposure is same as total exposure.
			total_exposure += bond_amount;
		}

		// enact the above changes
		Session::roll_until_active_era(2);
		// give rewards
		Staking::reward_by_ids(vec![(11, 1)]);

		// 100 nominators fit into 2 pages of exposure
		assert_eq!(MaxExposurePageSize::get(), 64);
		assert_eq!(Eras::<T>::exposure_page_count(2, &11), 2);

		// compute and ensure the reward amount is greater than zero.
		Session::roll_until_active_era(3);

		// verify the exposures are calculated correctly.
		let actual_exposure_0 = Eras::<T>::get_paged_exposure(2, &11, 0).unwrap();
		assert_eq!(actual_exposure_0.total(), total_exposure);
		assert_eq!(actual_exposure_0.own(), 1000);
		assert_eq!(actual_exposure_0.others().len(), 64);

		let actual_exposure_1 = Eras::<T>::get_paged_exposure(2, &11, 1).unwrap();
		assert_eq!(actual_exposure_1.total(), total_exposure);
		// own stake is only included once in the first page
		assert_eq!(actual_exposure_1.own(), 0);
		assert_eq!(actual_exposure_1.others().len(), 100 - 64);

		let pre_payout_total_issuance = pallet_balances::TotalIssuance::<T>::get();
		RewardOnUnbalanceWasCalled::set(false);

		// flush any events
		let _ = staking_events_since_last_call();

		let controller_balance_before_p0_payout = asset::stakeable_balance::<T>(&11);
		// Payout rewards for first exposure page
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0));

		// verify `Rewarded` events are being executed
		assert!(matches!(
			staking_events_since_last_call().as_slice(),
			&[
				Event::PayoutStarted { era_index: 2, validator_stash: 11, page: 0, next: Some(1) },
				..,
				Event::Rewarded { stash: 1063, dest: RewardDestination::Stash, amount: _ },
				Event::Rewarded { stash: 1064, dest: RewardDestination::Stash, amount: _ },
			]
		));

		let controller_balance_after_p0_payout = asset::stakeable_balance::<T>(&11);

		// verify the validator has been rewarded
		assert!(controller_balance_after_p0_payout > controller_balance_before_p0_payout);

		// Verify validator reward for page 0: with 0% commission, the validator's total
		// reward is proportional to their own stake. This is prorated across pages by
		// each page's stake fraction.
		let era_reward = Eras::<T>::get_stakers_reward(2).unwrap();
		let validator_total_reward =
			Perbill::from_rational(1000 as Balance, total_exposure).mul_floor(era_reward);
		let page_0_total = actual_exposure_0.page_total();
		let page_0_part = Perbill::from_rational(page_0_total, total_exposure);
		let expected_page_0_reward = page_0_part.mul_floor(validator_total_reward);
		let actual_validator_reward =
			controller_balance_after_p0_payout - controller_balance_before_p0_payout;
		assert_eq!(
			actual_validator_reward, expected_page_0_reward,
			"Validator page 0 reward: got {} expected {}",
			actual_validator_reward, expected_page_0_reward,
		);

		// Payout the second and last page of nominators
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 1));

		// verify `Rewarded` events for second page (validator is also rewarded on each page).
		let events = staking_events_since_last_call();
		assert!(matches!(
			events.as_slice(),
			&[
				Event::PayoutStarted { era_index: 2, validator_stash: 11, page: 1, next: None },
				Event::Rewarded { stash: 11, dest: RewardDestination::Stash, amount: _ },
				Event::Rewarded { stash: 1065, dest: RewardDestination::Stash, amount: _ },
				Event::Rewarded { stash: 1066, dest: RewardDestination::Stash, amount: _ },
				..
			]
		));

		// Validator is rewarded on every page (prorated by page stake).
		let controller_balance_after_p1_payout = asset::stakeable_balance::<T>(&11);
		assert!(controller_balance_after_p1_payout > controller_balance_after_p0_payout);

		// Total validator reward across both pages should equal the full own-stake reward.
		let total_validator_paid =
			controller_balance_after_p1_payout - controller_balance_before_p0_payout;
		assert_eq_error_rate!(total_validator_paid, validator_total_reward, 1);

		// In non-minting mode, payouts transfer from pot, so issuance is unchanged
		assert_eq!(pallet_balances::TotalIssuance::<T>::get(), pre_payout_total_issuance);

		// Verify total nominator rewards: sum of all nominator balance increases should
		// equal the nominator payout (within rounding tolerance).
		let total_nominator_reward: Balance = (0..100)
			.map(|i| {
				let nom_id = 1000 + i;
				let initial = balance + i as Balance;
				asset::stakeable_balance::<T>(&nom_id).saturating_sub(initial)
			})
			.sum();
		let expected_nominator_total = era_reward.saturating_sub(validator_total_reward);
		// Allow 1 per nominator rounding tolerance.
		assert_eq_error_rate!(total_nominator_reward, expected_nominator_total, 100);

		assert!(asset::stakeable_balance::<T>(&11) > balance);
		for i in 0..100 {
			assert!(asset::stakeable_balance::<T>(&(1000 + i)) > balance + i as Balance);
		}

		// verify rewards are tracked to prevent double claims
		for page in 0..Eras::<T>::exposure_page_count(2, &11) {
			assert_eq!(Eras::<T>::is_rewards_claimed(2, &11, page), true);
		}

		for i in 4..17 {
			Staking::reward_by_ids(vec![(11, 1)]);

			// compute and ensure the reward amount is greater than zero.
			let total_payout = total_payout_for(time_per_era());
			let pre_roll_issuance = pallet_balances::TotalIssuance::<T>::get();

			Session::roll_until_active_era(i);

			// Issuance should have increased by total_payout (staker + treasury rewards)
			assert_eq_error_rate!(
				pallet_balances::TotalIssuance::<T>::get(),
				pre_roll_issuance + total_payout,
				2
			);

			let post_roll_issuance = pallet_balances::TotalIssuance::<T>::get();
			mock::make_all_reward_payment(i - 1);
			// Payout just transfers from pot, so issuance doesnt change
			assert_eq!(pallet_balances::TotalIssuance::<T>::get(), post_roll_issuance);

			// verify we track rewards for each era and page
			for page in 0..Eras::<T>::exposure_page_count(i - 1, &11) {
				assert_eq!(Eras::<T>::is_rewards_claimed(i - 1, &11, page), true);
			}
		}

		assert_eq!(ClaimedRewards::<T>::get(14, &11), vec![0, 1]);

		let last_era = 99;
		let history_depth = HistoryDepth::get();
		let last_reward_era = last_era - 1;
		let first_claimable_reward_era = last_era - history_depth;
		for i in 17..=last_era {
			Staking::reward_by_ids(vec![(11, 1)]);
			// compute and ensure the reward amount is greater than zero.
			let _ = validator_payout_for(time_per_era());
			Session::roll_until_active_era(i);
		}

		// With manual pruning, old eras are NOT automatically cleaned up
		// ClaimedRewards for old eras will remain until manually pruned
		// Check eras that had rewards claimed (2-14, based on test setup)
		for era in 2..15 {
			// These eras had rewards and should still have ClaimedRewards since pruning is manual
			assert!(!ClaimedRewards::<T>::get(era, &11).is_empty());
		}

		// verify only page 0 is marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era,
			0
		));
		assert_eq!(ClaimedRewards::<T>::get(first_claimable_reward_era, &11), vec![0]);

		// verify page 0 and 1 are marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era,
			1
		));
		assert_eq!(ClaimedRewards::<T>::get(first_claimable_reward_era, &11), vec![0, 1]);

		// verify only page 0 is marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			last_reward_era,
			0
		));
		assert_eq!(ClaimedRewards::<T>::get(last_reward_era, &11), vec![0]);

		// verify page 0 and 1 are marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			last_reward_era,
			1
		));
		assert_eq!(ClaimedRewards::<T>::get(last_reward_era, &11), vec![0, 1]);

		// Out of order claims works.
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 69, 0));
		assert_eq!(ClaimedRewards::<T>::get(69, &11), vec![0]);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 23, 1));
		assert_eq!(ClaimedRewards::<T>::get(23, &11), vec![1]);
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 42, 0));
		assert_eq!(ClaimedRewards::<T>::get(42, &11), vec![0]);
	});
}

#[test]
fn test_multi_page_payout_stakers_backward_compatible() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Track the exposure of the validator and all nominators.
		let mut total_exposure = balance;
		// Create a validator:
		bond_validator(11, balance); // Default(64)
		assert_eq!(Validators::<T>::count(), 1);

		let err_weight = <T as Config>::WeightInfo::payout_stakers_alive_staked(0);

		// Create nominators, targeting stash of validators
		for i in 0..100 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
			// with multi page reward payout, payout exposure is same as total exposure.
			total_exposure += bond_amount;
		}

		Session::roll_until_active_era(2);
		Staking::reward_by_ids(vec![(11, 1)]);

		// Since `MaxExposurePageSize = 64`, there are two pages of validator exposure.
		assert_eq!(Eras::<T>::exposure_page_count(2, &11), 2);

		// compute and ensure the reward amount is greater than zero.
		Session::roll_until_active_era(3);

		// verify the exposures are calculated correctly.
		let actual_exposure_0 = Eras::<T>::get_paged_exposure(2, &11, 0).unwrap();
		assert_eq!(actual_exposure_0.total(), total_exposure);
		assert_eq!(actual_exposure_0.own(), 1000);
		assert_eq!(actual_exposure_0.others().len(), 64);

		let actual_exposure_1 = Eras::<T>::get_paged_exposure(2, &11, 1).unwrap();
		assert_eq!(actual_exposure_1.total(), total_exposure);
		// own stake is only included once in the first page
		assert_eq!(actual_exposure_1.own(), 0);
		assert_eq!(actual_exposure_1.others().len(), 100 - 64);

		let pre_payout_total_issuance = pallet_balances::TotalIssuance::<T>::get();
		RewardOnUnbalanceWasCalled::set(false);

		let controller_balance_before_p0_payout = asset::stakeable_balance::<T>(&11);
		// Payout rewards for first exposure page
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, 2));
		// page 0 is claimed
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		let controller_balance_after_p0_payout = asset::stakeable_balance::<T>(&11);

		// verify rewards have been paid out but still some left

		// verify the validator has been rewarded
		assert!(controller_balance_after_p0_payout > controller_balance_before_p0_payout);

		// This should payout the second and last page of nominators
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, 2));

		// cannot claim any more pages
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, 2),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		// Validator is rewarded on every page (prorated by page stake).
		assert!(asset::stakeable_balance::<T>(&11) > controller_balance_after_p0_payout);

		// verify all rewards have been paid out
		assert_eq_error_rate!(
			pallet_balances::TotalIssuance::<T>::get(),
			pre_payout_total_issuance,
			2
		);

		// verify all nominators of validator 11 are paid out, including the validator
		// Validator payout goes to controller.
		assert!(asset::stakeable_balance::<T>(&11) > balance);
		for i in 0..100 {
			assert!(asset::stakeable_balance::<T>(&(1000 + i)) > balance + i as Balance);
		}

		// verify rewards are tracked to prevent double claims
		for page in 0..Eras::<T>::exposure_page_count(2, &11) {
			assert_eq!(Eras::<T>::is_rewards_claimed(2, &11, page), true);
		}

		for i in 4..17 {
			Staking::reward_by_ids(vec![(11, 1)]);

			// compute and ensure the reward amount is greater than zero.
			let total_payout = total_payout_for(time_per_era());
			let pre_roll_issuance = pallet_balances::TotalIssuance::<T>::get();

			Session::roll_until_active_era(i);

			// Issuance should have increased by total_payout (staker + treasury rewards)
			assert_eq_error_rate!(
				pallet_balances::TotalIssuance::<T>::get(),
				pre_roll_issuance + total_payout,
				2
			);

			let post_roll_issuance = pallet_balances::TotalIssuance::<T>::get();
			mock::make_all_reward_payment(i - 1);
			// Payout just transfers from pot, so issuance doesnt change
			assert_eq!(pallet_balances::TotalIssuance::<T>::get(), post_roll_issuance);

			// verify we track rewards for each era and page
			for page in 0..Eras::<T>::exposure_page_count(i - 1, &11) {
				assert_eq!(Eras::<T>::is_rewards_claimed(i - 1, &11, page), true);
			}
		}

		assert_eq!(ClaimedRewards::<T>::get(14, &11), vec![0, 1]);

		let last_era = 99;
		let history_depth = HistoryDepth::get();
		let last_reward_era = last_era - 1;
		let first_claimable_reward_era = last_era - history_depth;
		for i in 17..=last_era {
			Staking::reward_by_ids(vec![(11, 1)]);
			// compute and ensure the reward amount is greater than zero.
			let _ = validator_payout_for(time_per_era());
			Session::roll_until_active_era(i);
		}

		// With manual pruning, old eras are NOT automatically cleaned up
		// ClaimedRewards for old eras will remain until manually pruned
		// Check eras that had rewards claimed (2-14, based on test setup)
		for era in 2..15 {
			// These eras had rewards and should still have ClaimedRewards since pruning is manual
			assert!(!ClaimedRewards::<T>::get(era, &11).is_empty());
		}

		// verify only page 0 is marked as claimed
		assert_ok!(Staking::payout_stakers(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era
		));
		assert_eq!(ClaimedRewards::<T>::get(first_claimable_reward_era, &11), vec![0]);

		// verify page 0 and 1 are marked as claimed
		assert_ok!(Staking::payout_stakers(
			RuntimeOrigin::signed(1337),
			11,
			first_claimable_reward_era,
		));
		assert_eq!(ClaimedRewards::<T>::get(first_claimable_reward_era, &11), vec![0, 1]);

		// change order and verify only page 1 is marked as claimed
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			last_reward_era,
			1
		));
		assert_eq!(ClaimedRewards::<T>::get(last_reward_era, &11), vec![1]);

		// verify page 0 is claimed even when explicit page is not passed
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, last_reward_era,));

		assert_eq!(ClaimedRewards::<T>::get(last_reward_era, &11), vec![1, 0]);

		// cannot claim any more pages
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, last_reward_era),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		// Create 4 nominator pages
		for i in 100..200 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
		}

		let test_era = last_era + 1;
		Session::roll_until_active_era(test_era);

		Staking::reward_by_ids(vec![(11, 1)]);
		// compute and ensure the reward amount is greater than zero.
		let _ = validator_payout_for(time_per_era());

		Session::roll_until_active_era(test_era + 1);

		// Out of order claims works.
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, test_era, 2));
		assert_eq!(ClaimedRewards::<T>::get(test_era, &11), vec![2]);

		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, test_era));
		assert_eq!(ClaimedRewards::<T>::get(test_era, &11), vec![2, 0]);

		// cannot claim page 2 again
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, test_era, 2),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, test_era));
		assert_eq!(ClaimedRewards::<T>::get(test_era, &11), vec![2, 0, 1]);

		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), 11, test_era));
		assert_eq!(ClaimedRewards::<T>::get(test_era, &11), vec![2, 0, 1, 3]);
	});
}

#[test]
fn test_page_count_and_size() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 1000;
		// Track the exposure of the validator and all nominators.
		// Create a validator:
		bond_validator(11, balance); // Default(64)
		assert_eq!(Validators::<T>::count(), 1);

		// Create nominators, targeting stash of validators
		for i in 0..100 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
		}

		Session::roll_until_active_era(2);

		// Since max exposure page size is 64, 2 pages of nominators are created.
		assert_eq!(MaxExposurePageSize::get(), 64);
		assert_eq!(Eras::<T>::exposure_page_count(2, &11), 2);

		// first page has 64 nominators
		assert_eq!(Eras::<T>::get_paged_exposure(2, &11, 0).unwrap().others().len(), 64);
		// second page has 36 nominators
		assert_eq!(Eras::<T>::get_paged_exposure(2, &11, 1).unwrap().others().len(), 36);

		// now lets decrease page size
		MaxExposurePageSize::set(32);

		Session::roll_until_active_era(3);

		// now we expect 4 pages.
		assert_eq!(Eras::<T>::exposure_page_count(3, &11), 4);
		// first 3 pages have 32 nominators each
		assert_eq!(Eras::<T>::get_paged_exposure(3, &11, 0).unwrap().others().len(), 32);
		assert_eq!(Eras::<T>::get_paged_exposure(3, &11, 1).unwrap().others().len(), 32);
		assert_eq!(Eras::<T>::get_paged_exposure(3, &11, 2).unwrap().others().len(), 32);
		assert_eq!(Eras::<T>::get_paged_exposure(3, &11, 3).unwrap().others().len(), 4);

		// now lets decrease page size even more
		MaxExposurePageSize::set(5);
		Session::roll_until_active_era(4);

		// now we expect the max 20 pages (100/5).
		assert_eq!(Eras::<T>::exposure_page_count(4, &11), 20);
	});
}

#[test]
fn payout_stakers_handles_basic_errors() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let err_weight = <T as Config>::WeightInfo::payout_stakers_alive_staked(0);

		// Same setup as the test above
		let balance = 1000;
		bond_validator(11, balance); // Default(64)

		// Create nominators, targeting stash
		for i in 0..100 {
			bond_nominator(1000 + i, balance + i as Balance, vec![11]);
		}

		Session::roll_until_active_era(2);
		Staking::reward_by_ids(vec![(11, 1)]);
		let _ = validator_payout_for(time_per_era());
		Session::roll_until_active_era(3);

		// Wrong Era, too big
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 3, 0),
			Error::<T>::InvalidEraToReward.with_weight(err_weight)
		);
		// Wrong Staker
		assert_noop!(
			Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 10, 2, 0),
			Error::<T>::NotStash.with_weight(err_weight)
		);

		let last_era = 99;
		for i in 4..=last_era {
			Staking::reward_by_ids(vec![(11, 1)]);
			// compute and ensure the reward amount is greater than zero.
			let _ = validator_payout_for(time_per_era());
			Session::roll_until_active_era(i);
		}

		let history_depth = HistoryDepth::get();
		let expected_last_reward_era = last_era - 1;
		let expected_start_reward_era = last_era - history_depth;

		// We are at era last_era=99. Given history_depth=80, we should be able
		// to payout era starting from expected_start_reward_era=19 through
		// expected_last_reward_era=98 (80 total eras), but not 18 or 99.
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_start_reward_era - 1,
				0
			),
			Error::<T>::InvalidEraToReward.with_weight(err_weight)
		);
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era + 1,
				0
			),
			Error::<T>::InvalidEraToReward.with_weight(err_weight)
		);
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			expected_start_reward_era,
			0
		));
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			expected_last_reward_era,
			0
		));

		// can call page 1
		assert_ok!(Staking::payout_stakers_by_page(
			RuntimeOrigin::signed(1337),
			11,
			expected_last_reward_era,
			1
		));

		// Can't claim again
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_start_reward_era,
				0
			),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era,
				0
			),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era,
				1
			),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);

		// invalid page
		assert_noop!(
			Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				11,
				expected_last_reward_era,
				2
			),
			Error::<T>::InvalidPage.with_weight(err_weight)
		);
	});
}

#[test]
fn test_commission_paid_across_pages() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		let balance = 10_000;
		let commission = 50;

		// Create a validator with significant self-stake to make own-stake reward visible.
		bond_validator(11, balance);
		assert_ok!(Staking::validate(
			RuntimeOrigin::signed(11),
			ValidatorPrefs { commission: Perbill::from_percent(commission), blocked: false }
		));
		assert_eq!(Validators::<T>::count(), 1);

		// Create nominators, targeting stash of validators
		for i in 0..200 {
			let bond_amount = balance + i as Balance;
			bond_nominator(1000 + i, bond_amount, vec![11]);
		}

		Session::roll_until_active_era(2);
		Staking::reward_by_ids(vec![(11, 1)]);

		// Since `MaxExposurePageSize = 64`, there are four pages of validator
		// exposure.
		assert_eq!(Eras::<T>::exposure_page_count(2, &11), 4);

		// compute and ensure the reward amount is greater than zero.
		let payout = validator_payout_for(time_per_era());
		Session::roll_until_active_era(3);

		let initial_balance = asset::stakeable_balance::<T>(&11);
		// Payout rewards for first exposure page
		assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, 0));

		let controller_balance_after_p0_payout = asset::stakeable_balance::<T>(&11);

		// some commission is paid
		assert!(initial_balance < controller_balance_after_p0_payout);

		// payout all pages
		for i in 1..4 {
			let before_balance = asset::stakeable_balance::<T>(&11);
			assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 2, i));
			let after_balance = asset::stakeable_balance::<T>(&11);
			// some commission is paid for every page
			assert!(before_balance < after_balance);
		}

		// The validator should receive:
		// - 50% commission on the total era reward (payout)
		// - proportional own-stake share of the remaining 50%
		let total_exposure: Balance =
			balance + (0..200).map(|i| balance + i as Balance).sum::<Balance>();
		let commission_reward = Perbill::from_percent(commission).mul_floor(payout);
		let leftover = payout.saturating_sub(commission_reward);
		let own_stake_reward = Perbill::from_rational(balance, total_exposure).mul_floor(leftover);
		let expected_total = commission_reward + own_stake_reward;
		let actual_total = asset::stakeable_balance::<T>(&11) - initial_balance;
		// Allow error of 1 per page due to floor rounding across 4 pages.
		assert_eq_error_rate!(actual_total, expected_total, 4);
	});
}

#[test]
fn payout_stakers_handles_weight_refund() {
	// Note: this test relies on the assumption that `payout_stakers_alive_staked` is solely used by
	// `payout_stakers` to calculate the weight of each payout op.
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		use crate::Call as StakingCall;
		let max_nom_rewarded = MaxExposurePageSize::get();

		// Make sure the configured value is meaningful for our use.
		assert!(max_nom_rewarded >= 4);
		let half_max_nom_rewarded = max_nom_rewarded / 2;

		// Sanity check our max and half max nominator quantities.
		assert!(half_max_nom_rewarded > 0);
		assert!(max_nom_rewarded > half_max_nom_rewarded);

		let max_nom_rewarded_weight =
			<T as Config>::WeightInfo::payout_stakers_alive_staked(max_nom_rewarded);
		let half_max_nom_rewarded_weight =
			<T as Config>::WeightInfo::payout_stakers_alive_staked(half_max_nom_rewarded);
		let zero_nom_payouts_weight = <T as Config>::WeightInfo::payout_stakers_alive_staked(0);

		assert!(zero_nom_payouts_weight.any_gt(Weight::zero()));
		assert!(half_max_nom_rewarded_weight.any_gt(zero_nom_payouts_weight));
		assert!(max_nom_rewarded_weight.any_gt(half_max_nom_rewarded_weight));

		let balance = 1000;
		bond_validator(11, balance);

		// Era 2
		Session::roll_until_active_era(2);

		// Reward just the validator.
		Staking::reward_by_ids(vec![(11, 1)]);

		// Add some `half_max_nom_rewarded` nominators who will start backing the validator in the
		// next era.
		for i in 0..half_max_nom_rewarded {
			bond_nominator((1000 + i).into(), balance + i as Balance, vec![11]);
		}

		// Era 3
		Session::roll_until_active_era(3);

		// Collect payouts when there are no nominators
		let call = RuntimeCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 2,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));

		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), zero_nom_payouts_weight);

		// The validator is not rewarded in this era; so there will be zero payouts to claim for
		// this era.

		// next era -- with nominators now
		Session::roll_until_active_era(4);

		// Collect payouts for an era where the validator did not receive any points.
		let call = RuntimeCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 3,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), zero_nom_payouts_weight);

		// Reward the validator and its nominators.
		Staking::reward_by_ids(vec![(11, 1)]);

		// Era 5
		Session::roll_until_active_era(5);

		// Collect payouts when the validator has `half_max_nom_rewarded` nominators.
		let call = RuntimeCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 4,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), half_max_nom_rewarded_weight);

		// Add enough nominators so that we are at the limit. They will be active nominators
		// in the next era.
		for i in half_max_nom_rewarded..max_nom_rewarded {
			bond_nominator((1000 + i).into(), balance + i as Balance, vec![11]);
		}

		// Era 6
		Session::roll_until_active_era(6);

		// We now have `max_nom_rewarded` nominators actively nominating our validator.
		// Reward the validator so we can collect for everyone in the next era.
		Staking::reward_by_ids(vec![(11, 1)]);

		// Era 7
		Session::roll_until_active_era(7);

		// Collect payouts when the validator had `half_max_nom_rewarded` nominators.
		let call = RuntimeCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 6,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert_ok!(result);
		assert_eq!(extract_actual_weight(&result, &info), max_nom_rewarded_weight);

		// Try and collect payouts for an era that has already been collected.
		let call = RuntimeCall::Staking(StakingCall::payout_stakers_by_page {
			validator_stash: 11,
			era: 6,
			page: 0,
		});
		let info = call.get_dispatch_info();
		let result = call.dispatch(RuntimeOrigin::signed(20));
		assert!(result.is_err());
		// When there is an error the consumed weight == weight when there are 0 nominator payouts.
		assert_eq!(extract_actual_weight(&result, &info), zero_nom_payouts_weight);
	});
}

#[test]
fn test_runtime_api_pending_rewards() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN
		let err_weight = <T as Config>::WeightInfo::payout_stakers_alive_staked(0);
		let stake = 100;

		// validator with non-paged exposure, rewards marked in legacy claimed rewards.
		let validator_one = 301;
		// validator with non-paged exposure, rewards marked in paged claimed rewards.
		let validator_two = 302;
		// validator with paged exposure.
		let validator_three = 303;

		// Set staker
		for v in validator_one..=validator_three {
			let _ = asset::set_stakeable_balance::<T>(&v, stake);
			assert_ok!(Staking::bond(RuntimeOrigin::signed(v), stake, RewardDestination::Staked));
		}

		// Add reward points
		let reward = EraRewardPoints {
			total: 1,
			individual: bounded_btree_map![validator_one => 1, validator_two => 1, validator_three => 1],
		};
		ErasRewardPoints::<T>::insert(0, reward);
		// Build exposure
		let mut individual_exposures: Vec<IndividualExposure<AccountId, Balance>> = vec![];
		for i in 0..=MaxExposurePageSize::get() {
			let nominator: AccountId = (10_000 + i).into();
			bond(nominator, stake);
			individual_exposures.push(IndividualExposure { who: nominator, value: stake });
		}
		let exposure = Exposure::<AccountId, Balance> {
			total: stake * (MaxExposurePageSize::get() as Balance + 2),
			own: stake,
			others: individual_exposures,
		};

		// add exposure for validators
		Eras::<T>::upsert_exposure(0, &validator_one, exposure.clone());
		Eras::<T>::upsert_exposure(0, &validator_two, exposure.clone());

		// add some reward to be distributed
		ErasValidatorReward::<T>::insert(0, 1000);

		// SCENARIO: Validator with paged exposure (two pages).
		// validators have not claimed rewards, so pending rewards is true.
		assert!(Eras::<T>::pending_rewards(0, &validator_one));
		assert!(Eras::<T>::pending_rewards(0, &validator_two));
		// and payout works
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_one, 0));
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_two, 0));
		// validators have two pages of exposure, so pending rewards is still true.
		assert!(Eras::<T>::pending_rewards(0, &validator_one));
		assert!(Eras::<T>::pending_rewards(0, &validator_two));
		// payout again only for validator one
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_one, 0));
		// now pending rewards is false for validator one
		assert!(!Eras::<T>::pending_rewards(0, &validator_one));
		// and payout fails for validator one
		assert_noop!(
			Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_one, 0),
			Error::<T>::AlreadyClaimed.with_weight(err_weight)
		);
		// while pending reward is true for validator two
		assert!(Eras::<T>::pending_rewards(0, &validator_two));
		// and payout works again for validator two.
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_two, 0));

		// SCENARIO: Validator with exposure but zero reward points (e.g. elected but authored no
		// blocks). Its payout transfers nothing, so pending_rewards is false.
		let validator_four = 304; // elected but earned zero points
		let _ = asset::set_stakeable_balance::<T>(&validator_four, stake);
		assert_ok!(Staking::bond(
			RuntimeOrigin::signed(validator_four),
			stake,
			RewardDestination::Staked
		));
		let exposure_four =
			Exposure::<AccountId, Balance> { total: stake, own: stake, others: Default::default() };
		Eras::<T>::upsert_exposure(0, &validator_four, exposure_four);

		// exposure exists and the page is unclaimed, yet the validator has no reward points.
		assert!(!ErasRewardPoints::<T>::get(0).individual.contains_key(&validator_four));
		assert!(!Eras::<T>::is_rewards_claimed(0, &validator_four, 0));
		assert!(!Eras::<T>::pending_rewards(0, &validator_four));

		// paying out succeeds but is a no-op: no `Rewarded` event is emitted for the validator.
		let _ = staking_events_since_last_call();
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator_four, 0));
		assert!(!staking_events_since_last_call().iter().any(|e| matches!(
			e,
			Event::Rewarded { stash, .. } if *stash == validator_four
		)));
		// the no-op payout still marks the (only) page claimed.
		assert!(Eras::<T>::is_rewards_claimed(0, &validator_four, 0));
		assert_eq!(ClaimedRewards::<T>::get(0, &validator_four).to_vec(), vec![0]);
	});
}

#[test]
fn legacy_payout_ignores_pot_account_existence() {
	ExtBuilder::default().legacy_reward_mode().build_and_execute(|| {
		let validator = 11; // validator

		// GIVEN: a legacy era with rewards to pay out, guard unset.
		Staking::reward_by_ids(vec![(validator, 1)]);
		Session::roll_until_active_era(2);

		let era = 1;
		let expected_stakers =
			(time_per_era() as Balance) - RemainderRatio::get() * (time_per_era() as Balance);
		assert_eq!(ErasValidatorReward::<Test>::get(era).unwrap(), expected_stakers);
		assert_eq!(DisableMintingGuard::<Test>::get(), None);

		// WHEN: the era pot account is made to exist externally (dust transfer at ED).
		let pot = SequentialTest::pot_account(RewardPot::Era(era, RewardKind::StakerRewards));
		asset::set_stakeable_balance::<Test>(&pot, asset::existential_deposit::<Test>());
		assert!(crate::reward::EraRewardManager::<Test>::has_staker_rewards_pot(era));

		// THEN: payout still takes the legacy (mint) path. Issuance growing by the full
		// staker reward means new tokens were minted rather than transferred from a pot.
		let pre_issuance = pallet_balances::TotalIssuance::<Test>::get();
		assert_ok!(Staking::payout_stakers(RuntimeOrigin::signed(1337), validator, era));
		let minted = pallet_balances::TotalIssuance::<Test>::get() - pre_issuance;
		assert_eq!(minted, expected_stakers);
	});
}
