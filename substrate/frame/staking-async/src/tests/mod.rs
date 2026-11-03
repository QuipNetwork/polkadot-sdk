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

//! Tests for the module.

use super::*;
use crate::{asset, ledger::StakingLedgerInspect, mock::Session};
use frame_election_provider_support::{
	bounds::{DataProviderBounds, ElectionBoundsBuilder},
	SortedListProvider,
};
use frame_support::{
	assert_noop, assert_ok, assert_storage_noop, hypothetically,
	pallet_prelude::*,
	traits::{InspectLockableCurrency, ReservableCurrency},
};
use mock::*;
use sp_runtime::{
	assert_eq_error_rate, bounded_vec, traits::BadOrigin, Perbill, Percent, TokenError,
};
use sp_staking::{Stake, StakingAccount, StakingInterface};
use substrate_test_utils::assert_eq_uvec;

mod bonding;
mod configs;
mod controller;
mod election_data_provider;
mod election_provider;
mod era_rotation;
mod force_unstake_kill_stash;
mod ledger;
mod legacy_reward;
mod nominators_no_slashing;
mod payout_stakers;
mod slashing;
mod try_state;
mod validator_incentive;

#[test]
fn basic_setup_session_queuing_should_work() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		assert_eq!(Session::current_index(), 3);

		// put some money in account that we'll use.
		for i in 1..5 {
			let _ = asset::set_stakeable_balance::<Test>(&i, 2000);
		}

		// add a new candidate for being a validator. account 3.
		assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 1500, RewardDestination::Account(3)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()));

		// No effects will be seen so far.
		assert_eq_uvec!(Session::validators(), vec![21, 11]);

		Session::roll_until_session(4);
		assert_eq_uvec!(Session::validators(), vec![21, 11]);
		assert_eq!(Session::queued_validators(), None);

		Session::roll_until_session(5);
		assert_eq_uvec!(Session::validators(), vec![21, 11]);
		assert_eq_uvec!(Session::queued_validators().unwrap(), vec![21, 3]);

		Session::roll_until_session(6);
		assert_eq_uvec!(Session::validators(), vec![21, 3]);
		assert_eq!(Session::queued_validators(), None);

		// then chill 3
		Staking::chill(RuntimeOrigin::signed(3)).unwrap();

		// nothing. 3 is still there.
		Session::roll_until_session(7);
		assert_eq_uvec!(Session::validators(), vec![21, 3]);

		Session::roll_until_session(8);
		assert_eq_uvec!(Session::validators(), vec![21, 3]);

		// now are back -- 3 is gone
		Session::roll_until_session(9);
		assert_eq_uvec!(Session::validators(), vec![21, 11]);

		// 3 is still bonded though
		assert_eq!(
			Staking::ledger(3.into()).unwrap(),
			StakingLedgerInspect {
				stash: 3,
				total: 1500,
				active: 1500,
				unlocking: Default::default(),
			}
		);

		// e.g. it cannot reserve more than 500 that it has free from the total 2000
		assert_noop!(Balances::reserve(&3, 501), DispatchError::ConsumerRemaining);
		assert_ok!(Balances::reserve(&3, 409));
	});
}

#[test]
fn basic_setup_works() {
	// Verifies initial conditions of mock
	ExtBuilder::default().build_and_execute(|| {
		// Account 11 is stashed and locked, and is the controller
		assert_eq!(Staking::bonded(&11), Some(11));
		// Account 21 is stashed and locked and is the controller
		assert_eq!(Staking::bonded(&21), Some(21));
		// Account 1 is not a stashed
		assert_eq!(Staking::bonded(&1), None);

		// Account 11 controls its own stash, which is 100 * balance_factor units
		assert_eq!(
			Ledger::get(&11).unwrap(),
			StakingLedgerInspect::<Test> {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);
		// Account 21 controls its own stash, which is 200 * balance_factor units
		assert_eq!(
			Ledger::get(&21).unwrap(),
			StakingLedgerInspect::<Test> {
				stash: 21,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);
		// Account 1 does not control any stash
		assert!(Staking::ledger(1.into()).is_err());

		// ValidatorPrefs are default
		assert_eq_uvec!(
			<Validators<Test>>::iter().collect::<Vec<_>>(),
			vec![
				(31, ValidatorPrefs::default()),
				(21, ValidatorPrefs::default()),
				(11, ValidatorPrefs::default())
			]
		);

		// check the single nominators we have
		assert_eq!(
			Staking::ledger(101.into()).unwrap(),
			StakingLedgerInspect {
				stash: 101,
				total: 500,
				active: 500,
				unlocking: Default::default(),
			}
		);
		assert_eq!(Nominators::<Test>::get(101).unwrap().targets, vec![11, 21]);

		assert_eq!(
			Staking::eras_stakers(active_era(), &11),
			Exposure {
				total: 1250,
				own: 1000,
				others: vec![IndividualExposure { who: 101, value: 250 }]
			},
		);
		assert_eq!(
			Staking::eras_stakers(active_era(), &21),
			Exposure {
				total: 1250,
				own: 1000,
				others: vec![IndividualExposure { who: 101, value: 250 }]
			},
		);

		// Current active and planned era
		assert_eq!(active_era(), 1);
		assert_eq!(current_era(), 1);
		assert_eq!(Session::current_index(), 3);
		assert_eq_uvec!(Session::validators(), vec![11, 21]);

		// initial total stake = 1125 + 1375
		assert_eq!(ErasTotalStake::<Test>::get(active_era()), 2500);

		// The number of validators required.
		assert_eq!(ValidatorCount::<Test>::get(), 2);

		// New era is not being forced
		assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);

		// Events so far
		assert_eq!(
			staking_events(),
			vec![
				Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 1 },
				Event::PagedElectionProceeded { page: 0, result: Ok(2) },
				Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 1 },
				Event::EraPaid { era_index: 0, validator_payout: 7500, remainder: 0 },
				Event::SessionRotated { starting_session: 3, active_era: 1, planned_era: 1 }
			]
		);
	});
}

#[test]
fn basic_setup_session_rotation() {
	ExtBuilder::default().build_and_execute(|| {
		// our initial clean state at active era 1.
		assert_eq!(active_era(), 1);
		assert_eq!(current_era(), 1);
		assert_eq!(Session::current_index(), 3);
		assert_eq_uvec!(Session::validators(), vec![11, 21]);

		// roll one session, we have planned our era
		Session::roll_to_next_session();
		assert_eq!(Session::current_index(), 4);
		assert_eq!(current_era(), 2);
		assert_eq!(active_era(), 1);

		// roll one session, still in planning, and something is queued in session now.
		Session::roll_to_next_session();
		assert_eq!(Session::current_index(), 5);
		assert_eq!(active_era(), 1);
		assert_eq!(current_era(), 2);

		// roll one session, we activate the era.
		Session::roll_to_next_session();
		assert_eq!(Session::current_index(), 6);
		assert_eq!(active_era(), 2);
		assert_eq!(current_era(), 2);
	});
}

#[test]
fn basic_setup_sessions_per_era() {
	ExtBuilder::default()
		.session_per_era(6)
		.no_flush_events()
		.build_and_execute(|| {
			// test state forwards us to the end of session 6 / active era 1
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 3, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 4, active_era: 0, planned_era: 1 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 5, active_era: 0, planned_era: 1 },
					Event::EraPaid { era_index: 0, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 6, active_era: 1, planned_era: 1 }
				]
			);
			assert_eq!(Session::current_index(), 6);
			assert_eq!(active_era(), 1);

			Session::roll_until_active_era(2);
			assert_eq!(Session::current_index(), 12);
			assert_eq!(active_era(), 2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 9, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 10, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 11, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 12, active_era: 2, planned_era: 2 }
				]
			);
		});
}

mod try_state_assertions {
	use super::*;
	#[test]
	#[should_panic = "called `Result::unwrap()` on an `Err` value: Other(\"number of entries in payee storage items does not match the number of bonded ledgers\")"]
	fn check_payee_invariant1_works() {
		// A bonded ledger should always have an assigned `Payee` This test should panic as we
		// verify that a bad state will panic due to the `try_state` checks in the `post_checks`
		// in `mock`.
		ExtBuilder::default().build_and_execute(|| {
			let rogue_ledger = StakingLedger::<Test>::new(123456, 20);
			Ledger::<Test>::insert(123456, rogue_ledger);
		})
	}

	#[test]
	#[should_panic = "called `Result::unwrap()` on an `Err` value: Other(\"number of entries in payee storage items does not match the number of bonded ledgers\")"]
	fn check_payee_invariant2_works() {
		// The number of entries in both `Payee` and of bonded staking ledgers should match. This
		// test should panic as we verify that a bad state will panic due to the `try_state`
		// checks in the `post_checks` in `mock`.
		ExtBuilder::default().build_and_execute(|| {
			Payee::<Test>::insert(1111, RewardDestination::Staked);
		})
	}
}

mod validator_count {
	use super::*;

	#[test]
	fn increase_validator_count_errors() {
		ExtBuilder::default().build_and_execute(|| {
			MaxValidatorSet::set(50);
			MaxWinnersPerPage::set(50);
			assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 40));

			// increase works
			assert_ok!(Staking::increase_validator_count(RuntimeOrigin::root(), 6));
			assert_eq!(ValidatorCount::<Test>::get(), 46);

			// errors
			assert_noop!(
				Staking::increase_validator_count(RuntimeOrigin::root(), 5),
				Error::<Test>::TooManyValidators,
			);
		})
	}

	#[test]
	fn scale_validator_count_errors() {
		ExtBuilder::default().build_and_execute(|| {
			MaxValidatorSet::set(50);
			MaxWinnersPerPage::set(50);
			assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 20));

			// scale value works
			assert_ok!(Staking::scale_validator_count(
				RuntimeOrigin::root(),
				Percent::from_percent(200)
			));
			assert_eq!(ValidatorCount::<Test>::get(), 40);

			// errors
			assert_noop!(
				Staking::scale_validator_count(RuntimeOrigin::root(), Percent::from_percent(126)),
				Error::<Test>::TooManyValidators,
			);
		})
	}

	#[test]
	fn cannot_set_unsupported_validator_count() {
		ExtBuilder::default().build_and_execute(|| {
			MaxValidatorSet::set(50);
			MaxWinnersPerPage::set(50);

			// set validator count works
			assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 30));
			assert_ok!(Staking::set_validator_count(RuntimeOrigin::root(), 50));

			// setting validator count above 100 does not work
			assert_noop!(
				Staking::set_validator_count(RuntimeOrigin::root(), 51),
				Error::<Test>::TooManyValidators,
			);
		})
	}
}

mod staking_interface {
	use frame_support::storage::with_storage_layer;
	use sp_staking::StakingInterface;

	use super::*;

	#[test]
	fn force_unstake_with_slash_works() {
		ExtBuilder::default().build_and_execute(|| {
			// without slash
			let _ = with_storage_layer::<(), _, _>(|| {
				// bond an account, can unstake
				assert_eq!(Staking::bonded(&11), Some(11));
				assert_ok!(<Staking as StakingInterface>::force_unstake(11));
				Err(DispatchError::from("revert"))
			});

			// bond again and add a slash, still can unstake.
			assert_eq!(Staking::bonded(&11), Some(11));
			add_slash(11);
			assert_ok!(<Staking as StakingInterface>::force_unstake(11));
		});
	}

	#[test]
	fn do_withdraw_unbonded_can_kill_stash_with_existential_deposit_zero() {
		ExtBuilder::default()
			.existential_deposit(0)
			.nominate(false)
			.build_and_execute(|| {
				// Initial state of 11
				assert_eq!(Staking::bonded(&11), Some(11));
				assert_eq!(
					Staking::ledger(11.into()).unwrap(),
					StakingLedgerInspect {
						stash: 11,
						total: 1000,
						active: 1000,
						unlocking: Default::default(),
					}
				);
				assert_eq!(
					Staking::eras_stakers(active_era(), &11),
					Exposure { total: 1000, own: 1000, others: vec![] }
				);

				// Unbond all of the funds in stash.
				Staking::chill(RuntimeOrigin::signed(11)).unwrap();
				Staking::unbond(RuntimeOrigin::signed(11), 1000).unwrap();
				assert_eq!(
					Staking::ledger(11.into()).unwrap(),
					StakingLedgerInspect {
						stash: 11,
						total: 1000,
						active: 0,
						unlocking: bounded_vec![UnlockChunk { value: 1000, era: 4 }],
					},
				);

				// trigger future era.
				Session::roll_until_active_era(4);

				// withdraw unbonded
				assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));

				// empty stash has been reaped
				assert!(!<Ledger<Test>>::contains_key(&11));
				assert!(!<Bonded<Test>>::contains_key(&11));
				assert!(!<Validators<Test>>::contains_key(&11));
				assert!(!<Payee<Test>>::contains_key(&11));
				// lock is removed.
				assert_eq!(asset::staked::<Test>(&11), 0);
			});
	}

	#[test]
	fn status() {
		ExtBuilder::default().build_and_execute(|| {
			// stash of a validator is identified as a validator
			assert_eq!(Staking::status(&11).unwrap(), StakerStatus::Validator);
			// .. but not the controller.
			assert!(Staking::status(&10).is_err());

			// stash of nominator is identified as a nominator
			assert_eq!(Staking::status(&101).unwrap(), StakerStatus::Nominator(vec![11, 21]));
			// .. but not the controller.
			assert!(Staking::status(&100).is_err());

			// stash of chilled is identified as a chilled
			assert_eq!(Staking::status(&41).unwrap(), StakerStatus::Idle);
			// .. but not the controller.
			assert!(Staking::status(&40).is_err());

			// random other account.
			assert!(Staking::status(&42).is_err());
		})
	}
}

mod staking_unchecked {
	use sp_staking::{Stake, StakingInterface, StakingUnchecked};

	use super::*;

	#[test]
	fn virtual_bond_does_not_lock_or_hold() {
		ExtBuilder::default().build_and_execute(|| {
			assert_eq!(asset::total_balance::<Test>(&10), 0);

			// 10 can bond more than its balance amount since we do not require lock for virtual
			// bonding.
			assert_ok!(<Staking as StakingUnchecked>::virtual_bond(&10, 100, &15));

			// nothing is locked on 10.
			assert_eq!(asset::staked::<Test>(&10), 0);

			// adding more balance does not lock anything as well.
			assert_ok!(<Staking as StakingInterface>::bond_extra(&10, 1000));

			// but ledger is updated correctly.
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 1100, active: 1100 })
			);

			// lets try unbonding some amount.
			assert_ok!(<Staking as StakingInterface>::unbond(&10, 200));

			assert_eq!(
				Staking::ledger(10.into()).unwrap(),
				StakingLedgerInspect {
					stash: 10,
					total: 1100,
					active: 1100 - 200,
					unlocking: bounded_vec![UnlockChunk { value: 200, era: 1 + 3 }],
				}
			);

			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 1100, active: 900 })
			);

			// still no locks.
			assert_eq!(asset::staked::<Test>(&10), 0);

			Session::roll_until_active_era(2);

			// cannot withdraw without waiting for unbonding period.
			assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(10, 0));
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 1100, active: 900 })
			);

			// in era 4, 10 can withdraw unlocking amount.
			Session::roll_until_active_era(4);
			assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(10, 0));
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 900, active: 900 })
			);

			// unbond all.
			assert_ok!(<Staking as StakingInterface>::unbond(&10, 900));
			assert_eq!(
				<Staking as StakingInterface>::stake(&10),
				Ok(Stake { total: 900, active: 0 })
			);

			Session::roll_until_active_era(7);
			assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(10, 0));

			// ensure withdrawing all amount cleans up storage.
			assert_eq!(Staking::ledger(10.into()), Err(Error::<Test>::NotStash));
			assert_eq!(VirtualStakers::<Test>::contains_key(10), false);
		})
	}

	#[test]
	fn virtual_staker_cannot_pay_reward_to_self_account() {
		ExtBuilder::default().build_and_execute(|| {
			// cannot set payee to self
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&10, 100, &10),
				Error::<Test>::RewardDestinationRestricted
			);

			// to another account works
			assert_ok!(<Staking as StakingUnchecked>::virtual_bond(&10, 100, &11));

			// cannot set via set_payee as well.
			assert_noop!(
				<Staking as StakingInterface>::set_payee(&10, &10),
				Error::<Test>::RewardDestinationRestricted
			);
		});
	}

	#[test]
	fn virtual_staker_cannot_bond_again() {
		ExtBuilder::default().build_and_execute(|| {
			// 200 virtual bonds
			bond_virtual_nominator(200, 201, 500, vec![11, 21]);

			// Tries bonding again
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&200, 200, &201),
				Error::<Test>::AlreadyBonded
			);

			// And again with a different reward destination.
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&200, 200, &202),
				Error::<Test>::AlreadyBonded
			);

			// Direct bond is not allowed as well.
			assert_noop!(
				<Staking as StakingInterface>::bond(&200, 200, &202),
				Error::<Test>::AlreadyBonded
			);
		});
	}

	#[test]
	fn normal_staker_cannot_virtual_bond() {
		ExtBuilder::default().build_and_execute(|| {
			// 101 is a nominator trying to virtual bond
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&101, 200, &102),
				Error::<Test>::AlreadyBonded
			);

			// validator 21 tries to virtual bond
			assert_noop!(
				<Staking as StakingUnchecked>::virtual_bond(&21, 200, &22),
				Error::<Test>::AlreadyBonded
			);
		});
	}

	#[test]
	fn migrate_virtual_staker() {
		ExtBuilder::default().build_and_execute(|| {
			// give some balance to 200
			asset::set_stakeable_balance::<Test>(&200, 2000);

			// stake
			assert_ok!(Staking::bond(RuntimeOrigin::signed(200), 1000, RewardDestination::Staked));
			assert_eq!(asset::staked::<Test>(&200), 1000);

			// migrate them to virtual staker
			assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&200));
			// payee needs to be updated to a non-stash account.
			assert_ok!(<Staking as StakingInterface>::set_payee(&200, &201));

			// ensure the balance is not locked anymore
			assert_eq!(asset::staked::<Test>(&200), 0);

			// and they are marked as virtual stakers
			assert_eq!(Pallet::<Test>::is_virtual_staker(&200), true);
		});
	}

	#[test]
	fn virtual_nominators_are_lazily_slashed() {
		ExtBuilder::default().build_and_execute(|| {
			let slash_percent = Perbill::from_percent(5);
			let initial_exposure = Staking::eras_stakers(active_era(), &11);
			// 101 is a nominator for 11
			assert_eq!(initial_exposure.others.first().unwrap().who, 101);
			// make 101 a virtual nominator
			assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&101));
			// set payee different to self.
			assert_ok!(<Staking as StakingInterface>::set_payee(&101, &102));

			// cache values
			let nominator_stake = Staking::ledger(101.into()).unwrap().active;
			let nominator_balance = asset::stakeable_balance::<Test>(&101);
			let validator_stake = Staking::ledger(11.into()).unwrap().active;
			let validator_balance = asset::stakeable_balance::<Test>(&11);
			let exposed_stake = initial_exposure.total;
			let exposed_validator = initial_exposure.own;
			let exposed_nominator = initial_exposure.others.first().unwrap().value;

			// 11 gets slashed
			add_slash_with_percent(11, 5);
			// so that slashes are applied
			Session::roll_next();

			let slash_amount = slash_percent * exposed_stake;
			let validator_share =
				Perbill::from_rational(exposed_validator, exposed_stake) * slash_amount;
			let nominator_share =
				Perbill::from_rational(exposed_nominator, exposed_stake) * slash_amount;

			// both slash amounts need to be positive for the test to make sense.
			assert!(validator_share > 0);
			assert!(nominator_share > 0);

			// both stakes must have been decreased pro-rata.
			assert_eq!(
				Staking::ledger(101.into()).unwrap().active,
				nominator_stake - nominator_share
			);
			assert_eq!(
				Staking::ledger(11.into()).unwrap().active,
				validator_stake - validator_share
			);

			// validator balance is slashed as usual
			assert_eq!(asset::stakeable_balance::<Test>(&11), validator_balance - validator_share);

			// but virtual nominator's balance is not slashed.
			assert_eq!(asset::stakeable_balance::<Test>(&101), nominator_balance);
			// but slash is broadcasted to slash observers.
			assert_eq!(SlashObserver::get().get(&101).unwrap(), &nominator_share);
		})
	}

	#[test]
	fn virtual_stakers_cannot_be_reaped() {
		ExtBuilder::default()
			.set_status(101, StakerStatus::Nominator(vec![11]))
			.build_and_execute(|| {
				// slash all stake.
				let initial_exposure = Staking::eras_stakers(active_era(), &11);
				// 101 is a nominator for 11
				assert_eq!(initial_exposure.others.first().unwrap().who, 101);
				// make 101 a virtual nominator
				assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&101));
				// set payee different to self.
				assert_ok!(<Staking as StakingInterface>::set_payee(&101, &102));

				// cache values
				let validator_balance = asset::stakeable_balance::<Test>(&11);
				let validator_stake = Staking::ledger(11.into()).unwrap().total;
				let nominator_balance = asset::stakeable_balance::<Test>(&101);
				let nominator_stake = Staking::ledger(101.into()).unwrap().total;

				// 11 gets slashed
				add_slash_with_percent(11, 100);
				// so that slashes are applied
				Session::roll_next();
				assert_eq!(
					staking_events_since_last_call(),
					vec![
						Event::OffenceReported {
							offence_era: 1,
							validator: 11,
							fraction: Perbill::from_percent(100),
						},
						Event::SlashComputed {
							offence_era: 1,
							slash_era: 1,
							offender: 11,
							page: 0
						},
						Event::Slashed { staker: 11, amount: 1000 },
						Event::Slashed { staker: 101, amount: 500 }
					]
				);

				// both stakes must have been decreased to 0.
				assert_eq!(Staking::ledger(11.into()).unwrap().active, 0);
				assert_eq!(Staking::ledger(101.into()).unwrap().active, 0);

				// all validator stake is slashed
				assert_eq_error_rate!(
					validator_balance - validator_stake,
					asset::stakeable_balance::<Test>(&11),
					1
				);

				// Virtual nominator's balance is not slashed.
				assert_eq!(asset::stakeable_balance::<Test>(&101), nominator_balance);
				// Slash is broadcasted to slash observers.
				assert_eq!(SlashObserver::get().get(&101).unwrap(), &nominator_stake);

				// validator can be reaped.
				assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(10), 11, 0));
				// nominator is a virtual staker and cannot be reaped.
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(10), 101, 0),
					Error::<Test>::VirtualStakerNotAllowed
				);
			})
	}

	#[test]
	fn restore_ledger_not_allowed_for_virtual_stakers() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);
			// 333 is corrupted
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// migrate to virtual staker.
			assert_ok!(<Staking as StakingUnchecked>::migrate_to_virtual_staker(&333));

			// recover the ledger won't work for virtual staker
			assert_noop!(
				Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None),
				Error::<Test>::VirtualStakerNotAllowed
			);

			// migrate 333 back to normal staker
			<VirtualStakers<Test>>::remove(333);

			// try restore again
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));
		})
	}
}

mod hold_migration {
	use super::*;

	#[test]
	fn ledger_update_creates_hold() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with old currency
			let alice = 300;
			bond_nominator(alice, 1000, vec![11]);
			assert_eq!(asset::staked::<Test>(&alice), 1000);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// migrate alice currency to legacy locks
			testing_utils::migrate_to_old_currency::<Test>(alice);
			// no more holds
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: 1000, active: 1000 })
			);

			// any ledger mutation should create a hold
			hypothetically!({
				// give some extra balance to alice.
				let _ = asset::mint_into_existing::<Test>(&alice, 100);

				// WHEN new fund is bonded to ledger.
				assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(alice), 100));

				// THEN new hold is created
				assert_eq!(asset::staked::<Test>(&alice), 1000 + 100);
				assert_eq!(
					<Staking as StakingInterface>::stake(&alice),
					Ok(Stake { total: 1100, active: 1100 })
				);

				// old locked balance is untouched
				assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			});

			hypothetically!({
				// WHEN new fund is unbonded from ledger.
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));

				// THEN hold is updated.
				assert_eq!(asset::staked::<Test>(&alice), 1000);
				assert_eq!(
					<Staking as StakingInterface>::stake(&alice),
					Ok(Stake { total: 1000, active: 900 })
				);

				// old locked balance is untouched
				assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			});

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN hold is updated.
			assert_eq!(asset::staked::<Test>(&alice), 1000);
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: 1000, active: 1000 })
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);

			// locked balance is removed
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
		});
	}

	#[test]
	fn migrate_removes_old_lock() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with old currency
			let alice = 300;
			bond_nominator(alice, 1000, vec![11]);
			testing_utils::migrate_to_old_currency::<Test>(alice);
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 1000);
			let pre_migrate_consumer = System::consumers(&alice);
			let _ = staking_events_since_last_call();

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN
			// the extra consumer from old code is removed.
			assert_eq!(System::consumers(&alice), pre_migrate_consumer - 1);
			// ensure no lock
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// ensure stake and hold are same.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: 1000, active: 1000 })
			);
			assert_eq!(asset::staked::<Test>(&alice), 1000);
			// ensure events are emitted.
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::CurrencyMigrated { stash: alice, force_withdraw: 0 }]
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);
		});
	}
	#[test]
	fn cannot_hold_all_stake() {
		// When there is not enough funds to hold all stake, part of the stake if force withdrawn.
		// At end of the migration, the stake and hold should be same.
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// GIVEN alice who is a nominator with old currency.
			let alice = 300;
			let stake = 1000;
			bond_nominator(alice, stake, vec![11]);
			testing_utils::migrate_to_old_currency::<Test>(alice);
			assert_eq!(asset::staked::<Test>(&alice), 0);
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), stake);
			// ledger has 1000 staked.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: stake, active: stake })
			);

			// Get rid of the extra ED to emulate all their balance including ED is staked.
			assert_ok!(Balances::transfer_allow_death(
				RuntimeOrigin::signed(alice),
				10,
				ExistentialDeposit::get()
			));

			let expected_force_withdraw = ExistentialDeposit::get();

			// ledger mutation would fail in this case before migration because of failing hold.
			assert_noop!(
				Staking::unbond(RuntimeOrigin::signed(alice), 100),
				Error::<Test>::NotEnoughFunds
			);

			// clear events
			let _ = staking_events_since_last_call();

			// WHEN alice currency is migrated.
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), alice));

			// THEN
			let expected_hold = stake - expected_force_withdraw;
			// ensure no lock
			assert_eq!(Balances::balance_locked(STAKING_ID, &alice), 0);
			// ensure stake and hold are same.
			assert_eq!(
				<Staking as StakingInterface>::stake(&alice),
				Ok(Stake { total: expected_hold, active: expected_hold })
			);
			assert_eq!(asset::staked::<Test>(&alice), expected_hold);
			// ensure events are emitted.
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::CurrencyMigrated {
					stash: alice,
					force_withdraw: expected_force_withdraw
				}]
			);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), alice),
				Error::<Test>::AlreadyMigrated
			);

			// unbond works after migration.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));
		});
	}

	#[test]
	fn virtual_staker_consumer_provider_dec() {
		// Ensure virtual stakers consumer and provider count is decremented.
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			// 200 virtual bonds
			bond_virtual_nominator(200, 201, 500, vec![11, 21]);

			// previously the virtual nominator had a provider inc by the delegation system as
			// well as a consumer by this pallet.
			System::inc_providers(&200);
			System::inc_consumers(&200).expect("has provider, can consume");

			hypothetically!({
				// migrate 200
				assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), 200));

				// ensure account does not exist in system anymore.
				assert_eq!(System::consumers(&200), 0);
				assert_eq!(System::providers(&200), 0);
				assert!(!System::account_exists(&200));

				// ensure cannot migrate again.
				assert_noop!(
					Staking::migrate_currency(RuntimeOrigin::signed(1), 200),
					Error::<Test>::AlreadyMigrated
				);
			});

			hypothetically!({
				// 200 has an erroneously extra provider
				System::inc_providers(&200);

				// causes migration to fail.
				assert_noop!(
					Staking::migrate_currency(RuntimeOrigin::signed(1), 200),
					Error::<Test>::BadState
				);
			});

			// 200 is funded for more than ED by a random account.
			assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(999), 200, 10));

			// it has an extra provider now.
			assert_eq!(System::providers(&200), 2);

			// migrate 200
			assert_ok!(Staking::migrate_currency(RuntimeOrigin::signed(1), 200));

			// 1 provider is left, consumers is 0.
			assert_eq!(System::providers(&200), 1);
			assert_eq!(System::consumers(&200), 0);

			// ensure cannot migrate again.
			assert_noop!(
				Staking::migrate_currency(RuntimeOrigin::signed(1), 200),
				Error::<Test>::AlreadyMigrated
			);
		});
	}
}

// #[test]
// fn reward_validator_slashing_validator_does_not_overflow() {
// ExtBuilder::default().nominate(false).build_and_execute(|| {
// let stake = u64::MAX as Balance * 2;
// let reward_slash = u64::MAX as Balance * 2;
//
// Assert multiplication overflows in balance arithmetic.
// assert!(stake.checked_mul(reward_slash).is_none());
//
// Set staker
// let _ = asset::set_stakeable_balance::<Test>(&11, stake);
//
// let reward = EraRewardPoints::<AccountId> {
// total: 1,
// individual: vec![(11, 1)].into_iter().collect(),
// };
//
// Check reward
// ErasRewardPoints::<Test>::insert(0, reward);
//
// force exposure metadata to account for the overflowing `stake`.
// ErasStakersOverview::<Test>::insert(
// current_era(),
// 11,
// PagedExposureMetadata { total: stake, own: stake, nominator_count: 0, page_count: 0 },
// );
//
// we want to slash only self-stake, confirm that no others exposed.
// let full_exposure_after = Eras::<Test>::get_full_exposure(current_era(), &11);
// assert_eq!(full_exposure_after.total, stake);
// assert_eq!(full_exposure_after.others, vec![]);
//
// ErasValidatorReward::<Test>::insert(0, stake);
// assert_ok!(Staking::payout_stakers_by_page(RuntimeOrigin::signed(1337), 11, 0, 0));
// assert_eq!(asset::stakeable_balance::<Test>(&11), stake * 2);
//
// ensure ledger has `stake` and no more.
// Ledger::<Test>::insert(
// 11,
// StakingLedgerInspect {
// stash: 11,
// total: stake,
// active: stake,
// unlocking: Default::default(),
// },
// );
// Set staker (unsafe, can reduce balance below actual stake)
// let _ = asset::set_stakeable_balance::<Test>(&11, stake);
// let _ = asset::set_stakeable_balance::<Test>(&2, stake);
//
// only slashes out of bonded stake are applied. without this line, it is 0.
// Staking::bond(RuntimeOrigin::signed(2), stake - 1, RewardDestination::Staked).unwrap();
//
// Override metadata and exposures of 11 so that it exposes minmal self stake and `stake` -
// 1 from nominator 2.
// ErasStakersOverview::<Test>::insert(
// current_era(),
// 11,
// PagedExposureMetadata { total: stake, own: 1, nominator_count: 1, page_count: 1 },
// );
//
// ErasStakersPaged::<Test>::insert(
// (current_era(), &11, 0),
// ExposurePage {
// page_total: stake - 1,
// others: vec![IndividualExposure { who: 2, value: stake - 1 }],
// },
// );
//
// Check slashing
// on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(100)], true);
//
// assert_eq!(asset::stakeable_balance::<Test>(&11), stake - 1);
// assert_eq!(asset::stakeable_balance::<Test>(&2), 1);
// })
// }
//
// #[test]
// fn validator_is_not_disabled_for_an_offence_in_previous_era() {
// ExtBuilder::default()
// .validator_count(4)
// .set_status(41, StakerStatus::Validator)
// .build_and_execute(|| {
// mock::start_active_era(1);
//
// assert!(<Validators<Test>>::contains_key(11));
// assert!(Session::validators().contains(&11));
//
// on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(0)], true);
//
// assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
// assert!(is_disabled(11));
//
// mock::start_active_era(2);
//
// the validator is not disabled in the new era
// Staking::validate(RuntimeOrigin::signed(11), Default::default()).unwrap();
// assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
// assert!(<Validators<Test>>::contains_key(11));
// assert!(Session::validators().contains(&11));
//
// mock::start_active_era(3);
//
// an offence committed in era 1 is reported in era 3
// on_offence_in_era(&[offence_from(11, None)], &[Perbill::from_percent(0)], 1, true);
//
// the validator doesn't get disabled for an old offence
// assert!(Validators::<Test>::iter().any(|(stash, _)| stash == 11));
// assert!(!is_disabled(11));
//
// and we are not forcing a new era
// assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
//
// on_offence_in_era(
// &[offence_from(11, None)],
// NOTE: A 100% slash here would clean up the account, causing de-registration.
// &[Perbill::from_percent(95)],
// 1,
// true,
// );
//
// the validator doesn't get disabled again
// assert!(Validators::<Test>::iter().any(|(stash, _)| stash == 11));
// assert!(!is_disabled(11));
// and we are still not forcing a new era
// assert_eq!(ForceEra::<Test>::get(), Forcing::NotForcing);
// });
// }
//
// #[test]
// fn slash_kicks_validators_not_nominators_and_disables_nominator_for_kicked_validator() {
// ExtBuilder::default()
// .validator_count(7)
// .set_status(41, StakerStatus::Validator)
// .set_status(51, StakerStatus::Validator)
// .set_status(201, StakerStatus::Validator)
// .set_status(202, StakerStatus::Validator)
// .build_and_execute(|| {
// mock::start_active_era(1);
// assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 41, 51, 201, 202]);
//
// pre-slash balance
// assert_eq!(asset::stakeable_balance::<Test>(&11), 1000);
// assert_eq!(asset::stakeable_balance::<Test>(&101), 2000);
//
// 100 has approval for 11 as of now
// assert!(Nominators::<Test>::get(101).unwrap().targets.contains(&11));
//
// 11 and 21 both have the support of 100
// let exposure_11 = Staking::eras_stakers(active_era(), &11);
// let exposure_21 = Staking::eras_stakers(active_era(), &21);
//
// assert_eq!(exposure_11.total, 1000 + 125);
// assert_eq!(exposure_21.total, 1000 + 375);
//
// on_offence_now(&[offence_from(11, None)], &[Perbill::from_percent(10)], true);
//
// assert_eq!(
// staking_events_since_last_call(),
// vec![
// Event::PagedElectionProceeded { page: 0, result: Ok(7) },
// Event::StakersElected,
// Event::EraPaid { era_index: 0, validator_payout: 11075, remainder: 33225 },
// Event::OffenceReported {
// validator: 11,
// fraction: Perbill::from_percent(10),
// offence_era: 1
// },
// Event::SlashComputed { offence_era: 1, slash_era: 1, offender: 11, page: 0 },
// Event::Slashed { staker: 11, amount: 100 },
// Event::Slashed { staker: 101, amount: 12 },
// ]
// );
//
// assert!(matches!(
// session_events().as_slice(),
// &[.., SessionEvent::ValidatorDisabled { validator: 11 }]
// ));
//
// post-slash balance
// let nominator_slash_amount_11 = 125 / 10;
// assert_eq!(asset::stakeable_balance::<Test>(&11), 900);
// assert_eq!(asset::stakeable_balance::<Test>(&101), 2000 - nominator_slash_amount_11);
//
// check that validator was disabled.
// assert!(is_disabled(11));
//
// actually re-bond the slashed validator
// assert_ok!(Staking::validate(RuntimeOrigin::signed(11), Default::default()));
//
// mock::start_active_era(2);
// let exposure_11 = Staking::eras_stakers(active_era(), &11);
// let exposure_21 = Staking::eras_stakers(active_era(), &21);
//
// 11's own expo is reduced. sum of support from 11 is less (448), which is 500
// 900 + 146
// assert!(matches!(exposure_11, Exposure { own: 900, total: 1046, .. }));
// 1000 + 342
// assert!(matches!(exposure_21, Exposure { own: 1000, total: 1342, .. }));
// assert_eq!(500 - 146 - 342, nominator_slash_amount_11);
// });
// }
