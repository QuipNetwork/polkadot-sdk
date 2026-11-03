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

//! Tests concerning bond, bond_extra, unbond, rebond, withdraw and chill for stakers.

use super::*;
use frame_support::{hypothetically_ok, traits::Currency};

#[test]
fn existing_stash_cannot_bond() {
	ExtBuilder::default().build_and_execute(|| {
		assert!(StakingLedger::<T>::is_bonded(11.into()));

		// cannot bond again.
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(11), 7, RewardDestination::Staked),
			Error::<T>::AlreadyBonded,
		);
	});
}

#[test]
fn existing_controller_cannot_bond() {
	ExtBuilder::default().build_and_execute(|| {
		// `create_unique_stash_controller` bonds `ED * (balance_factor / 10).max(1)`. Pass a
		// `balance_factor` large enough that the bonded amount clears `min_chilled_bond` under
		// the default builder. The test's concern is the `AlreadyPaired` path, not the min-bond
		// thresholds.
		let (_stash, controller) = testing_utils::create_unique_stash_controller::<T>(
			0,
			100,
			RewardDestination::Staked,
			false,
		)
		.unwrap();

		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(controller), 7, RewardDestination::Staked),
			Error::<T>::AlreadyPaired,
		);
	});
}

#[test]
fn cannot_transfer_staked_balance() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		assert_eq!(asset::staked::<T>(&11), 1000);
		// stake + ed
		assert_eq!(asset::total_balance::<T>(&11), 1000 + 1);
		// nothing more to stake
		assert_eq!(asset::stakeable_balance::<T>(&11), 1000);
		assert_eq!(asset::free_to_stake::<T>(&11), 0);

		// cannot transfer
		assert_noop!(
			Balances::transfer_allow_death(RuntimeOrigin::signed(11), 21, 1),
			TokenError::Frozen,
		);

		let _ = asset::set_stakeable_balance::<T>(&11, 10000);

		// now it can
		assert_ok!(Balances::transfer_allow_death(RuntimeOrigin::signed(11), 21, 1));
	});
}

#[test]
fn cannot_reserve_staked_balance() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(asset::staked::<T>(&11), 1000);

		// Confirm account 11 cannot reserve as a result
		assert_noop!(
			Balances::reserve(&11, 2),
			pallet_balances::Error::<T, _>::InsufficientBalance
		);
		assert_noop!(Balances::reserve(&11, 1), DispatchError::ConsumerRemaining);

		// Give account 11 extra free balance
		let _ = asset::set_stakeable_balance::<T>(&11, 1000 + 1000);
		assert_eq!(asset::free_to_stake::<T>(&11), 1000);

		// Confirm account 11 can now reserve balance
		assert_ok!(Balances::reserve(&11, 500));

		// free to stake balance has reduced
		assert_eq!(asset::free_to_stake::<T>(&11), 500);
	});
}

#[test]
fn cannot_bond_less_than_ed() {
	ExtBuilder::default().existential_deposit(10).build_and_execute(|| {
		// given
		assert_eq!(asset::staked_and_not::<T>(&1), (0, 10));

		// cannot bond less than existential deposit
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(1), 9, RewardDestination::Staked),
			Error::<T>::InsufficientBond,
		);

		// can bond existential deposit
		assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 10, RewardDestination::Staked));
		assert_eq!(asset::staked_and_not::<T>(&1), (10, 0));
	});
}

#[test]
fn do_not_die_when_active_is_ed() {
	// `withdraw_unbonded` must not kill a stash whose remaining `active` is at the
	// existential deposit, even when both `MinValidatorBond` and `MinNominatorBond` sit strictly
	// above ED. The default `ExtBuilder` already supplies that configuration
	// (ED < `min_nominator_bond`  < `min_validator_bond`)
	let ed = 1;
	ExtBuilder::default()
		.existential_deposit(ed)
		.balance_factor(ed)
		.build_and_execute(|| {
			// given a stash whose ledger.active is above any min bond.
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 1000 * ed,
					active: 1000 * ed,
					unlocking: Default::default(),
				}
			);
			// 21 must chill first: as a validator, unbonding below `min_validator_bond` is
			// rejected.
			assert_ok!(Staking::chill(RuntimeOrigin::signed(21)));

			// when unbonding all of it except ed.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(21), 999 * ed));

			Session::roll_until_active_era(4);
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(21), 0));

			// then the ledger is still there, sitting exactly at ED.
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: ed,
					active: ed,
					unlocking: Default::default(),
				}
			);
		})
}

#[test]
fn bond_truncated_to_maximum_possible() {
	ExtBuilder::default().build_and_execute(|| {
		// given
		assert_eq!(asset::free_to_stake::<T>(&1), 10);

		// then bonding 100 is equal to bonding 10
		assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 100, RewardDestination::Staked));
		assert_eq!(Staking::ledger(1.into()).unwrap().total, 10);
	});
}

#[test]
fn bond_extra_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// given
		asset::set_stakeable_balance::<T>(&11, 1000000);

		// when
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), 100));

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 1000 + 100,
				unlocking: Default::default(),
			}
		);

		// when
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), Balance::max_value()));

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000000,
				active: 1000000,
				unlocking: Default::default(),
			}
		);
	});
}

#[test]
fn bond_extra_controller_bad_state_works() {
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		assert_eq!(StakingLedger::<T>::get(StakingAccount::Stash(31)).unwrap().stash, 31);

		// simulate ledger in bad state: the controller 41 is associated to the stash 31 and 41.
		Bonded::<T>::insert(31, 41);

		// we confirm that the ledger is in bad state: 31 has 41 as controller and when fetching
		// the ledger associated with the controller 41, its stash is 41 (and not 31).
		assert_eq!(Ledger::<T>::get(41).unwrap().stash, 41);

		// if the ledger is in this bad state, the `bond_extra` should fail.
		// TODO: remove this BadState, we should no longer have it at all.
		assert_noop!(Staking::bond_extra(RuntimeOrigin::signed(31), 10), Error::<T>::BadState);
	})
}

#[test]
fn bond_extra_updates_exposure_later_if_exposed() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// given
		assert_eq!(active_era(), 1);
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

		// when
		asset::set_stakeable_balance::<T>(&11, 1000000);
		Staking::bond_extra(RuntimeOrigin::signed(11), 100).unwrap();

		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 1000 + 100,
				unlocking: Default::default(),
			}
		);
		// Exposure is a snapshot! only updated after the next era update.
		assert_ne!(
			Staking::eras_stakers(active_era(), &11),
			Exposure { total: 1000 + 100, own: 1000 + 100, others: vec![] }
		);

		// when trigger next era
		Session::roll_until_active_era(2);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000 + 100,
				active: 1000 + 100,
				unlocking: Default::default(),
			}
		);
		// Exposure is now updated
		assert_eq!(
			Staking::eras_stakers(active_era(), &11),
			Exposure { total: 1000 + 100, own: 1000 + 100, others: vec![] }
		);
	})
}

#[test]
fn cannot_bond_extra_to_lower_than_ed() {
	ExtBuilder::default()
		.existential_deposit(11)
		.balance_factor(11)
		.build_and_execute(|| {
			// initial stuff.
			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 11 * 1000,
					active: 11 * 1000,
					unlocking: Default::default(),
				}
			);

			// unbond all of it. must be chilled first.
			assert_ok!(Staking::chill(RuntimeOrigin::signed(21)));
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(21), 11 * 1000));

			assert_eq!(
				Staking::ledger(21.into()).unwrap(),
				StakingLedgerInspect {
					stash: 21,
					total: 11 * 1000,
					active: 0,
					unlocking: bounded_vec![UnlockChunk {
						value: 11 * 1000,
						era: active_era() + 3
					}],
				}
			);

			// now bond a wee bit more
			assert_noop!(
				Staking::bond_extra(RuntimeOrigin::signed(21), 5),
				Error::<Test>::InsufficientBond,
			);
		})
}

#[test]
fn unbonding_works() {
	ExtBuilder::default().build_and_execute(|| {
		// given
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// when
		Staking::unbond(RuntimeOrigin::signed(11), 500).unwrap();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Unbonded { stash: 11, amount: 500, era: active_era() + 3 }]
		);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: active_era() + 3 }],
			},
		);

		// when
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(staking_events_since_last_call(), vec![]);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: active_era() + 3 }],
			},
		);

		// when
		Session::roll_until_active_era(2);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: 1 + 3 }],
			},
		);

		// when
		Session::roll_until_active_era(3);
		let _ = staking_events_since_last_call();
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(staking_events_since_last_call(), vec![]);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: 1 + 3 }],
			},
		);

		// when
		Session::roll_until_active_era(4);
		let _ = staking_events_since_last_call();
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Withdrawn { stash: 11, amount: 500 }]
		);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect { stash: 11, total: 500, active: 500, unlocking: bounded_vec![] },
		);
	});
}

#[test]
fn unbonding_multi_chunk() {
	ExtBuilder::default().build_and_execute(|| {
		// given
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// when
		Staking::unbond(RuntimeOrigin::signed(11), 500).unwrap();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Unbonded { stash: 11, amount: 500, era: active_era() + 3 }]
		);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: active_era() + 3 }],
			},
		);

		// when
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();
		Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Unbonded { stash: 11, amount: 250, era: active_era() + 3 }]
		);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 250,
				unlocking: bounded_vec![
					UnlockChunk { value: 500, era: 1 + 3 },
					UnlockChunk { value: 250, era: 2 + 3 }
				],
			},
		);

		// when
		Session::roll_until_active_era(4);
		let _ = staking_events_since_last_call();
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Withdrawn { stash: 11, amount: 500 }]
		);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 500,
				active: 250,
				unlocking: bounded_vec![UnlockChunk { value: 250, era: 2 + 3 }],
			},
		);

		// when
		Session::roll_until_active_era(5);
		let _ = staking_events_since_last_call();
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Withdrawn { stash: 11, amount: 250 }]
		);

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect { stash: 11, total: 250, active: 250, unlocking: bounded_vec![] },
		);
	});
}

#[test]
fn unbond_same_era_merges_chunk_but_emits_per_unbond_era() {
	// Same-era unbonds merge into one chunk; each still emits its own event with that era.
	ExtBuilder::default().build_and_execute(|| {
		Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
		Staking::unbond(RuntimeOrigin::signed(11), 200).unwrap();

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::Unbonded { stash: 11, amount: 300, era: active_era() + 3 },
				Event::Unbonded { stash: 11, amount: 200, era: active_era() + 3 },
			]
		);

		// One merged chunk, same era as the events.
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: active_era() + 3 }],
			},
		);
	});
}

#[test]
fn full_unbonding_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(asset::free_to_stake::<T>(&11), 0);
		// fully unbonding a validator auto-chills first
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1000));
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::Chilled { stash: 11 },
				Event::Unbonded { stash: 11, amount: 1000, era: active_era() + 3 }
			]
		);

		// wait 3 eras
		Session::roll_until_active_era(active_era() + 3);
		let _ = staking_events_since_last_call();

		// done
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::StakerRemoved { stash: 11 }, Event::Withdrawn { stash: 11, amount: 1000 }]
		);

		// storage is clean, balance is unheld
		StakingLedger::<T>::assert_stash_killed(11);
		assert_eq!(asset::free_to_stake::<T>(&11), 1000);
	});
}

/// Test that full unbond auto-chills and removes the validator from the set.
/// This test was added in https://github.com/paritytech/polkadot-sdk/pull/3811.
#[test]
fn unbond_with_chill_works() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Set payee to stash.
		assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));

		// Give account 11 some large free balance greater than total
		let _ = asset::stakeable_balance::<T>(&11);
		asset::set_stakeable_balance::<T>(&11, 1000000);

		// confirm that 11 is a normal validator
		assert!(Validators::<T>::contains_key(11));
		let initial_validator_count = Validators::<T>::count();

		// Initial state of 11
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default()
			}
		);

		Session::roll_until_active_era(2);
		assert_eq!(active_era(), 2);

		let _ = staking_events_since_last_call();

		// Unbond all amount - should auto-chill
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1000));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::Chilled { stash: 11 },
				Event::Unbonded { stash: 11, amount: 1000, era: active_era() + 3 }
			]
		);

		// Validator is removed from the set
		assert!(!Validators::<T>::contains_key(11));
		assert!(Nominators::<T>::get(11).is_none());
		assert_eq!(Validators::<T>::count(), initial_validator_count - 1);
	});
}

#[test]
fn unbonding_merges_if_era_exists() {
	ExtBuilder::default().build_and_execute(|| {
		// given
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// when
		Staking::unbond(RuntimeOrigin::signed(11), 500).unwrap();

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 500,
				unlocking: bounded_vec![UnlockChunk { value: 500, era: 1 + 3 }],
			},
		);

		// when
		Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 250,
				unlocking: bounded_vec![UnlockChunk { value: 500 + 250, era: 1 + 3 }],
			},
		);
	});
}

#[test]
fn unbonding_rejects_if_max_chunks() {
	ExtBuilder::default()
		.max_unlock_chunks(3)
		.bonding_duration(7)
		.build_and_execute(|| {
			// given
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// when
			Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();
			Session::roll_until_active_era(2);
			Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();
			Session::roll_until_active_era(3);
			Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();

			// then
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 250,
					unlocking: bounded_vec![
						UnlockChunk { value: 250, era: 1 + 7 },
						UnlockChunk { value: 250, era: 2 + 7 },
						UnlockChunk { value: 250, era: 3 + 7 },
					],
				},
			);

			// when
			Session::roll_until_active_era(4);
			assert_noop!(Staking::unbond(RuntimeOrigin::signed(11), 100), Error::<T>::NoMoreChunks,);
		});
}

#[test]
fn unbonding_auto_withdraws_if_any() {
	ExtBuilder::default().max_unlock_chunks(3).build_and_execute(|| {
		// given
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// when
		Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();
		Session::roll_until_active_era(2);
		Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();
		Session::roll_until_active_era(3);
		Staking::unbond(RuntimeOrigin::signed(11), 250).unwrap();

		// then
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 250,
				unlocking: bounded_vec![
					UnlockChunk { value: 250, era: 1 + 3 },
					UnlockChunk { value: 250, era: 2 + 3 },
					UnlockChunk { value: 250, era: 3 + 3 },
				],
			},
		);

		// when
		Session::roll_until_active_era(4);
		// then they can unbond more, as it does auto withdraw of the first chunk
		Staking::unbond(RuntimeOrigin::signed(11), 100).unwrap();
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 750,
				active: 150,
				unlocking: bounded_vec![
					UnlockChunk { value: 250, era: 2 + 3 },
					UnlockChunk { value: 250, era: 3 + 3 },
					UnlockChunk { value: 100, era: 4 + 3 },
				],
			},
		);
	});
}

#[test]
fn unbonding_caps_to_ledger_active() {
	ExtBuilder::default()
		.nominate(false)
		.set_status(11, StakerStatus::Idle)
		.build_and_execute(|| {
			// given
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// when
			Staking::unbond(RuntimeOrigin::signed(11), 1500).unwrap();

			// then
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 0,
					unlocking: bounded_vec![UnlockChunk { value: 1000, era: 1 + 3 }],
				}
			);
		});
}

#[test]
fn unbond_avoids_dust() {
	ExtBuilder::default()
		.existential_deposit(5)
		.nominate(false)
		.set_status(11, StakerStatus::Idle)
		.build_and_execute(|| {
			// given
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// when
			Staking::unbond(RuntimeOrigin::signed(11), 998).unwrap();

			// then
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 0,
					unlocking: bounded_vec![UnlockChunk { value: 1000, era: 1 + 3 }],
				}
			);
		});
}

#[test]
fn unbond_rejects_if_min_role_bond_not_met() {
	ExtBuilder::default().min_validator_bond(100).build_and_execute(|| {
		// given
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		// then
		assert_noop!(Staking::unbond(RuntimeOrigin::signed(11), 950), Error::<T>::InsufficientBond);

		// can unbond to a value less than 100 remaining
		hypothetically_ok!(Staking::unbond(RuntimeOrigin::signed(11), 850));

		hypothetically!({
			// can also chill and then unbond more.
			assert_ok!(Staking::chill(RuntimeOrigin::signed(11)));
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 950));
		})
	})
}

#[test]
fn reducing_max_unlocking_chunks_abrupt() {
	// Concern is on validators only
	ExtBuilder::default().build_and_execute(|| {
		// given a staker at era=10 and MaxUnlockChunks set to 2
		MaxUnlockingChunks::set(2);
		Session::roll_until_active_era(10);

		assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 300, RewardDestination::Staked));
		assert!(matches!(Staking::ledger(3.into()), Ok(_)));

		// when staker unbonds
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 20));

		// then an unlocking chunk is added at `current_era + bonding_duration`
		// => 10 + 3 = 13
		let expected_unlocking: BoundedVec<UnlockChunk<Balance>, MaxUnlockingChunks> =
			bounded_vec![UnlockChunk { value: 20 as Balance, era: 13 as EraIndex }];
		assert!(matches!(Staking::ledger(3.into()),
			Ok(StakingLedger {
				unlocking,
				..
			}) if unlocking == expected_unlocking));

		// when staker unbonds at next era
		Session::roll_until_active_era(11);

		assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 50));

		// then another unlock chunk is added
		let expected_unlocking: BoundedVec<UnlockChunk<Balance>, MaxUnlockingChunks> =
			bounded_vec![UnlockChunk { value: 20, era: 13 }, UnlockChunk { value: 50, era: 14 }];
		assert!(matches!(Staking::ledger(3.into()),
			Ok(StakingLedger {
				unlocking,
				..
			}) if unlocking == expected_unlocking));

		// when staker unbonds further
		Session::roll_until_active_era(12);

		// then further unbonding not possible
		assert_noop!(Staking::unbond(RuntimeOrigin::signed(3), 20), Error::<Test>::NoMoreChunks);

		// when max unlocking chunks is reduced abruptly to a low value
		MaxUnlockingChunks::set(1);

		// then unbond, rebond ops are blocked with ledger in corrupt state
		assert_noop!(Staking::unbond(RuntimeOrigin::signed(3), 20), Error::<Test>::NotController);
		assert_noop!(Staking::rebond(RuntimeOrigin::signed(3), 100), Error::<Test>::NotController);

		// reset the ledger corruption
		MaxUnlockingChunks::set(2);

		// now rebond works again
		assert_ok!(Staking::rebond(RuntimeOrigin::signed(3), 20));
	})
}

#[test]
fn switching_roles() {
	// Test that it should be possible to switch between roles (nominator, validator, idle)
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// Reset reward destination
		for i in &[11, 21] {
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(*i), RewardDestination::Stash));
		}

		assert_eq_uvec!(Session::validators(), vec![21, 11]);

		// put some money in account that we'll use.
		for i in 1..7 {
			let _ = Balances::deposit_creating(&i, 5000);
		}

		// add a new validator candidate
		assert_ok!(Staking::bond(RuntimeOrigin::signed(5), 1000, RewardDestination::Account(5)));
		assert_ok!(Staking::validate(RuntimeOrigin::signed(5), ValidatorPrefs::default()));

		// add 2 nominators
		assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 2000, RewardDestination::Account(1)));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(1), vec![11, 5]));

		assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 500, RewardDestination::Account(3)));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![21]));

		Session::roll_until_active_era(2);

		// with current nominators 11 and 5 have the most stake
		assert_eq_uvec!(Session::validators(), vec![5, 11]);

		// 2 decides to be a validator, and 3 backs them. Consequences:
		assert_ok!(Staking::validate(RuntimeOrigin::signed(1), ValidatorPrefs::default()));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![21, 1]));

		// new stakes:
		// 11: 1000 self vote
		// 21: 1000 self vote + 250 vote
		// 5 : 1000 self vote
		// 1 : 2000 self vote + 250 vote.
		// Winners: 21 and 1

		Session::roll_until_active_era(3);

		assert_eq_uvec!(Session::validators(), vec![1, 21]);
	});
}

#[test]
fn bond_with_no_staked_value() {
	// Behavior when someone bonds with no staked value.
	// Particularly when they votes and the candidate is elected.
	ExtBuilder::default()
		.validator_count(3)
		.existential_deposit(5)
		.balance_factor(5)
		.nominate(false)
		.build_and_execute(|| {
			// Can't bond with 1
			assert_noop!(
				Staking::bond(RuntimeOrigin::signed(1), 1, RewardDestination::Account(1)),
				Error::<Test>::InsufficientBond,
			);
			// bonded with absolute minimum value possible.
			assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 5, RewardDestination::Account(1)));
			assert_eq!(pallet_balances::Holds::<Test>::get(&1)[0].amount, 5);

			// unbonding even 1 will cause all to be unbonded.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(1), 1));
			assert_eq!(
				Staking::ledger(1.into()).unwrap(),
				StakingLedgerInspect {
					stash: 1,
					active: 0,
					total: 5,
					unlocking: bounded_vec![UnlockChunk { value: 5, era: 4 }],
				}
			);

			Session::roll_until_active_era(2);
			Session::roll_until_active_era(3);

			// not yet removed.
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(1), 0));
			assert!(Staking::ledger(1.into()).is_ok());
			assert_eq!(pallet_balances::Holds::<Test>::get(&1)[0].amount, 5);

			Session::roll_until_active_era(4);

			// poof. Account 1 is removed from the staking system.
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(1), 0));
			assert!(Staking::ledger(1.into()).is_err());
			assert_eq!(pallet_balances::Holds::<Test>::get(&1).len(), 0);
		});
}

#[test]
fn bond_with_little_staked_value_bounded() {
	// This test exercises a "stingy" validator bonded at exactly ED. The `ExtBuilder` defaults
	// would block that with `InsufficientBond`, so here we set instead `min_*_bond == ED`.
	ExtBuilder::default()
		.min_nominator_bond(ExistentialDeposit::get())
		.min_validator_bond(ExistentialDeposit::get())
		.validator_count(3)
		.nominate(false)
		.build_and_execute(|| {
			assert_ok!(Staking::chill(RuntimeOrigin::signed(31)));
			assert_ok!(Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Stash));

			// Stingy validator.
			assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 1, RewardDestination::Account(1)));
			assert_ok!(Staking::validate(RuntimeOrigin::signed(1), ValidatorPrefs::default()));

			reward_all_elected();
			Session::roll_until_active_era(2);
			let _ = staking_events_since_last_call();
			mock::make_all_reward_payment(1);

			// 1 is elected.
			assert_eq_uvec!(session_validators(), vec![21, 11, 1]);

			// Old ones are rewarded.
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::PayoutStarted { era_index: 1, validator_stash: 11, page: 0, next: None },
					Event::Rewarded { stash: 11, dest: RewardDestination::Stash, amount: 2499 },
					Event::PayoutStarted { era_index: 1, validator_stash: 21, page: 0, next: None },
					Event::Rewarded { stash: 21, dest: RewardDestination::Staked, amount: 2499 },
					Event::PayoutStarted { era_index: 1, validator_stash: 31, page: 0, next: None },
					Event::Rewarded { stash: 31, dest: RewardDestination::Staked, amount: 2499 }
				]
			);

			// reward era 2
			reward_all_elected();
			Session::roll_until_active_era(3);
			let _ = staking_events_since_last_call();
			mock::make_all_reward_payment(2);

			// 1 is also rewarded
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::PayoutStarted { era_index: 2, validator_stash: 1, page: 0, next: None },
					Event::Rewarded { stash: 1, dest: RewardDestination::Account(1), amount: 2499 },
					Event::PayoutStarted { era_index: 2, validator_stash: 11, page: 0, next: None },
					Event::Rewarded { stash: 11, dest: RewardDestination::Stash, amount: 2499 },
					Event::PayoutStarted { era_index: 2, validator_stash: 21, page: 0, next: None },
					Event::Rewarded { stash: 21, dest: RewardDestination::Staked, amount: 2499 }
				]
			);

			assert_eq_uvec!(session_validators(), vec![21, 11, 1]);
			assert_eq!(Staking::eras_stakers(active_era(), &1).total, 1);
		});
}

#[test]
fn restricted_accounts_can_only_withdraw() {
	ExtBuilder::default().build_and_execute(|| {
		// alice is a non blacklisted account.
		let alice = 301;
		let _ = Balances::make_free_balance_be(&alice, 500);
		// alice can bond
		assert_ok!(Staking::bond(RuntimeOrigin::signed(alice), 100, RewardDestination::Staked));

		// and bob is a blacklisted account
		let bob = 302;
		let _ = Balances::make_free_balance_be(&bob, 500);
		restrict(&bob);

		// Bob cannot bond
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(bob), 100, RewardDestination::Staked,),
			Error::<Test>::Restricted
		);

		// alice is blacklisted now and cannot bond anymore
		restrict(&alice);

		assert_noop!(
			Staking::bond_extra(RuntimeOrigin::signed(alice), 100),
			Error::<Test>::Restricted
		);

		// but she can unbond her existing bond
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 100));

		// she cannot rebond the unbonded amount
		Session::roll_until_active_era(2);
		assert_noop!(Staking::rebond(RuntimeOrigin::signed(alice), 50), Error::<Test>::Restricted);

		// move to era when alice fund can be withdrawn
		Session::roll_until_active_era(5);

		// alice can withdraw now
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(alice), 0));

		// she still cannot bond
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(alice), 100, RewardDestination::Staked,),
			Error::<Test>::Restricted
		);

		// bob is removed from restrict list
		remove_from_restrict_list(&bob);

		// bob can bond now
		assert_ok!(Staking::bond(RuntimeOrigin::signed(bob), 100, RewardDestination::Staked));

		// and bond extra
		assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(bob), 100));

		Session::roll_until_active_era(6);

		// unbond also works.
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(bob), 100));

		// bob can withdraw as well.
		Session::roll_until_active_era(6);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(bob), 0));
	})
}

mod rebond {
	use super::*;

	#[test]
	fn rebond_works() {
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// given
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// nothing to rebond
			assert_noop!(
				Staking::rebond(RuntimeOrigin::signed(11), 500),
				Error::<Test>::NoUnlockChunk
			);

			// given
			Staking::unbond(RuntimeOrigin::signed(11), 900).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 100,
					unlocking: bounded_vec![UnlockChunk { value: 900, era: 1 + 3 }],
				}
			);

			// then rebond all the funds unbonded.
			Staking::rebond(RuntimeOrigin::signed(11), 900).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// Unbond almost all of the funds in stash.
			Staking::unbond(RuntimeOrigin::signed(11), 900).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 100,
					unlocking: bounded_vec![UnlockChunk { value: 900, era: 1 + 3 }],
				}
			);

			// Re-bond part of the funds unbonded.
			Staking::rebond(RuntimeOrigin::signed(11), 500).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 600,
					unlocking: bounded_vec![UnlockChunk { value: 400, era: 1 + 3 }],
				}
			);

			// Re-bond the remainder of the funds unbonded.
			Staking::rebond(RuntimeOrigin::signed(11), 500).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// Unbond parts of the funds in stash.
			Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
			Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
			Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 100,
					unlocking: bounded_vec![UnlockChunk { value: 900, era: 1 + 3 }],
				}
			);

			// Re-bond part of the funds unbonded.
			Staking::rebond(RuntimeOrigin::signed(11), 500).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 600,
					unlocking: bounded_vec![UnlockChunk { value: 400, era: 1 + 3 }],
				}
			);
		})
	}

	#[test]
	fn rebond_is_fifo() {
		ExtBuilder::default().build_and_execute(|| {
			// given
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 1000,
					unlocking: Default::default(),
				}
			);

			// Unbond some of the funds in stash.
			Staking::unbond(RuntimeOrigin::signed(11), 400).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 600,
					unlocking: bounded_vec![UnlockChunk { value: 400, era: 1 + 3 }],
				}
			);

			Session::roll_until_active_era(2);

			// Unbond more of the funds in stash.
			Staking::unbond(RuntimeOrigin::signed(11), 300).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 300,
					unlocking: bounded_vec![
						UnlockChunk { value: 400, era: 1 + 3 },
						UnlockChunk { value: 300, era: 2 + 3 },
					],
				}
			);

			Session::roll_until_active_era(3);

			// Unbond yet more of the funds in stash.
			Staking::unbond(RuntimeOrigin::signed(11), 200).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 100,
					unlocking: bounded_vec![
						UnlockChunk { value: 400, era: 1 + 3 },
						UnlockChunk { value: 300, era: 2 + 3 },
						UnlockChunk { value: 200, era: 3 + 3 },
					],
				}
			);

			// Re-bond half of the unbonding funds.
			Staking::rebond(RuntimeOrigin::signed(11), 400).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 500,
					unlocking: bounded_vec![
						UnlockChunk { value: 400, era: 1 + 3 },
						UnlockChunk { value: 100, era: 2 + 3 },
					],
				}
			);
		})
	}

	#[test]
	fn rebond_emits_right_value_in_event() {
		// When a user calls rebond with more than can be rebonded, things succeed,
		// and the rebond event emits the actual value rebonded.
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// Give account 11 some large free balance greater than total
			let _ = asset::set_stakeable_balance::<Test>(&11, 1000000);

			// Unbond almost all of the funds in stash.
			Staking::unbond(RuntimeOrigin::signed(11), 900).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 100,
					unlocking: bounded_vec![UnlockChunk { value: 900, era: 1 + 3 }],
				}
			);
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::Unbonded { stash: 11, amount: 900, era: active_era() + 3 }]
			);

			// Re-bond less than the total
			Staking::rebond(RuntimeOrigin::signed(11), 100).unwrap();
			assert_eq!(
				Staking::ledger(11.into()).unwrap(),
				StakingLedgerInspect {
					stash: 11,
					total: 1000,
					active: 200,
					unlocking: bounded_vec![UnlockChunk { value: 800, era: 1 + 3 }],
				}
			);
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::Bonded { stash: 11, amount: 100 }]
			);

			// Re-bond way more than available
			Staking::rebond(RuntimeOrigin::signed(11), 100_000).unwrap();
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
				staking_events_since_last_call(),
				vec![Event::Bonded { stash: 11, amount: 800 }]
			);
		});
	}

	#[test]
	fn cannot_rebond_to_lower_than_ed() {
		ExtBuilder::default()
			.existential_deposit(11)
			.balance_factor(11)
			.build_and_execute(|| {
				// initial stuff.
				assert_eq!(
					Staking::ledger(21.into()).unwrap(),
					StakingLedgerInspect {
						stash: 21,
						total: 11 * 1000,
						active: 11 * 1000,
						unlocking: Default::default(),
					}
				);

				// unbond all of it. must be chilled first.
				assert_ok!(Staking::chill(RuntimeOrigin::signed(21)));
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(21), 11 * 1000));

				assert_eq!(
					Staking::ledger(21.into()).unwrap(),
					StakingLedgerInspect {
						stash: 21,
						total: 11 * 1000,
						active: 0,
						unlocking: bounded_vec![UnlockChunk { value: 11 * 1000, era: 4 }],
					}
				);

				// now bond a wee bit more
				assert_noop!(
					Staking::rebond(RuntimeOrigin::signed(21), 5),
					Error::<Test>::InsufficientBond
				);
			})
	}
}

mod reap {
	use super::*;

	#[test]
	fn reap_stash_only_when_ledger_below_ed() {
		// `reap_stash` is gated by the existential deposit, not by `min_chilled_bond`. A stash
		// remains safe from permissionless reap as long as its `ledger.total >= ED`, regardless of
		// how `MinValidatorBond` / `MinNominatorBond` move.
		ExtBuilder::default()
			.min_nominator_bond(1_000)
			.min_validator_bond(1_500)
			.existential_deposit(10)
			.balance_factor(10)
			.build_and_execute(|| {
				// GIVEN: 11 is a bonded validator.
				assert_eq!(asset::staked::<Test>(&11), 10 * 1000);
				assert_eq!(Staking::bonded(&11), Some(11));

				assert!(<Ledger<Test>>::contains_key(&11));
				assert!(<Bonded<Test>>::contains_key(&11));
				assert!(<Validators<Test>>::contains_key(&11));
				assert!(<Payee<Test>>::contains_key(&11));

				// WHEN attempting to reap a fully funded stash.
				// THEN it is rejected.
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
					Error::<Test>::FundedTarget
				);

				// WHEN the ledger is tweaked to sit below both min bonds but still above ED.
				// (No easy way to drive the real balance below ED, so we patch the ledger
				// directly.)
				Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 999));

				// THEN reap is still rejected: ledger.total (999) >= ED (10).
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
					Error::<Test>::FundedTarget
				);

				// WHEN the ledger sits exactly at ED.
				Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 10));

				// THEN reap is still rejected.
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
					Error::<Test>::FundedTarget
				);

				// WHEN the ledger drops below ED.
				Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 9));

				// THEN the stash is reapable.
				assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0));

				// And all staking state is gone.
				assert!(!<Ledger<Test>>::contains_key(&11));
				assert!(!<Bonded<Test>>::contains_key(&11));
				assert!(!<Validators<Test>>::contains_key(&11));
				assert!(!<Payee<Test>>::contains_key(&11));
				assert_eq!(asset::staked::<Test>(&11), 0);
			});
	}

	#[test]
	fn reap_stash_works_with_existential_deposit_zero() {
		ExtBuilder::default()
			.existential_deposit(0)
			.balance_factor(10)
			.build_and_execute(|| {
				// GIVEN a bonded validator on a chain with ED = 0.
				assert_eq!(asset::staked::<Test>(&11), 10 * 1000);
				assert_eq!(Staking::bonded(&11), Some(11));

				assert!(<Ledger<Test>>::contains_key(&11));
				assert!(<Bonded<Test>>::contains_key(&11));
				assert!(<Validators<Test>>::contains_key(&11));
				assert!(<Payee<Test>>::contains_key(&11));

				// WHEN attempting to reap. THEN rejected: ledger is funded.
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
					Error::<Test>::FundedTarget
				);

				// WHEN the ledger is forced to zero.
				// (No easy way to cause an account to go below ED, we tweak their staking ledger
				// instead).
				Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 0));

				// THEN reap succeeds via the `is_zero()` branch (since ED = 0 makes `< ED`
				// vacuously false).
				assert_ok!(Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0));

				assert!(!<Ledger<Test>>::contains_key(&11));
				assert!(!<Bonded<Test>>::contains_key(&11));
				assert!(!<Validators<Test>>::contains_key(&11));
				assert!(!<Payee<Test>>::contains_key(&11));
				assert_eq!(asset::staked::<Test>(&11), 0);
			});
	}

	#[test]
	fn raising_min_bonds_does_not_make_non_dust_stash_reapable() {
		// Regression test: a governance change that raises `MinValidatorBond` and/or
		// `MinNominatorBond` must not turn existing, non-dust stashes into permissionlessly
		// reapable targets. The reap gate is the existential deposit, not `min_chilled_bond`.
		ExtBuilder::default()
			.existential_deposit(10)
			.balance_factor(10)
			.build_and_execute(|| {
				// GIVEN: 11 is a bonded validator with above-ED ledger.
				Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 100));
				assert!(<Ledger<Test>>::contains_key(&11));
				assert!(<Validators<Test>>::contains_key(&11));

				// stash is not reapable
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
					Error::<Test>::FundedTarget
				);

				let scenarios: &[(ConfigOp<Balance>, ConfigOp<Balance>)] = &[
					// only MinValidatorBond raised
					(ConfigOp::Noop, ConfigOp::Set(10_000)),
					// only MinNominatorBond raised
					(ConfigOp::Set(10_000), ConfigOp::Noop),
					// both raised
					(ConfigOp::Set(500), ConfigOp::Set(10_000)),
				];

				for (min_nominator, min_validator) in scenarios {
					hypothetically!({
						// WHEN governance applies this min-bond configuration.
						assert_ok!(Staking::set_staking_configs(
							RuntimeOrigin::root(),
							min_nominator.clone(),
							min_validator.clone(),
							ConfigOp::Noop,
							ConfigOp::Noop,
							ConfigOp::Noop,
							ConfigOp::Noop,
							ConfigOp::Noop,
							ConfigOp::Noop,
							ConfigOp::Noop,
						));

						// THEN the stash is still NOT reapable — ledger.total (100) >= ED (10).
						assert_noop!(
							Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
							Error::<Test>::FundedTarget
						);
						assert!(<Ledger<Test>>::contains_key(&11));
						assert!(<Bonded<Test>>::contains_key(&11));
						assert!(<Payee<Test>>::contains_key(&11));
					});
				}
			});
	}

	#[test]
	fn pending_slash_validator_with_non_dust_ledger_cannot_self_reap() {
		// A validator with a pending `UnappliedSlash` and a
		// non-dust ledger must not be reapable.
		ExtBuilder::default()
			.existential_deposit(10)
			.balance_factor(10)
			.build_and_execute(|| {
				// GIVEN a validator whose own bond is small but non-dust.
				Ledger::<Test>::insert(11, StakingLedger::<Test>::new(11, 100));

				// WHEN MinValidatorBond is raised far above the validator's bond.
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Noop,
					ConfigOp::Set(10_000),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// AND an offence is queued against the validator.
				add_slash(11);

				// THEN the validator (or anyone) cannot reap the stash before the slash applies.
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(11), 11, 0),
					Error::<Test>::FundedTarget
				);
				assert_noop!(
					Staking::reap_stash(RuntimeOrigin::signed(20), 11, 0),
					Error::<Test>::FundedTarget
				);
				assert!(<Ledger<Test>>::contains_key(&11));
			});
	}
}

mod nominate {
	use super::*;
	#[test]
	fn duplicate_nominations_stripped() {
		ExtBuilder::default().nominate(false).set_stake(31, 1000).build_and_execute(|| {
			// ensure all have equal stake.
			assert_eq!(
				<Validators<Test>>::iter()
					.map(|(v, _)| (v, Staking::ledger(v.into()).unwrap().total))
					.collect::<Vec<_>>(),
				vec![(31, 1000), (21, 1000), (11, 1000)],
			);

			// no nominators shall exist.
			assert!(<Nominators<T>>::iter().map(|(n, _)| n).collect::<Vec<_>>().is_empty());

			bond_nominator(1, 1000, vec![11, 11, 11, 21, 31]);
			assert_eq!(
				Nominators::<T>::get(1).unwrap(),
				Nominations {
					targets: bounded_vec![11, 21, 31],
					submitted_in: 1,
					suppressed: false
				}
			);
		});
	}

	#[test]
	fn nominating_non_validators_is_not_ok() {
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// given existing validators
			assert_eq!(
				<Validators<Test>>::iter().map(|(v, _)| v).collect::<Vec<_>>(),
				vec![31, 21, 11,],
			);

			// .. and no existing nominators
			assert!(<Nominators<T>>::iter().count() == 0);
			// and 1 bonded.
			assert_ok!(Staking::bond(RuntimeOrigin::signed(1), 1000, RewardDestination::Stash));

			// then
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(1), vec![41]),
				Error::<Test>::BadTarget
			);
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(1), vec![31, 21, 11, 41]),
				Error::<Test>::BadTarget
			);
		});
	}

	#[test]
	fn blocking_and_kicking_works() {
		ExtBuilder::default().validator_count(4).nominate(true).build_and_execute(|| {
			// given
			assert_ok!(Staking::validate(
				RuntimeOrigin::signed(11),
				ValidatorPrefs { blocked: true, ..Default::default() }
			));

			// attempt to nominate from 101
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(101), vec![11]));

			// should have worked since we're already nominated them
			assert_eq!(Nominators::<Test>::get(&101).unwrap().targets, vec![11]);

			// kick the nominator
			assert_ok!(Staking::kick(RuntimeOrigin::signed(11), vec![101]));

			// should have been kicked now
			assert!(Nominators::<Test>::get(&101).unwrap().targets.is_empty());

			// attempt to nominate from 100/101...
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(101), vec![11]),
				Error::<Test>::BadTarget
			);
		});
	}
}

mod staking_bounds_chill_other {
	use super::*;

	#[test]
	fn min_bond_checks_work() {
		ExtBuilder::default()
			.existential_deposit(100)
			.balance_factor(100)
			.min_nominator_bond(1_000)
			.min_validator_bond(1_500)
			.build_and_execute(|| {
				// 50 is not enough for any role (less than ED)
				assert_noop!(
					Staking::bond(RuntimeOrigin::signed(3), 50, RewardDestination::Stash),
					Error::<Test>::InsufficientBond
				);
				// 1000 is enough for nominator but not for validator.
				assert_ok!(Staking::bond(RuntimeOrigin::signed(3), 1000, RewardDestination::Stash));
				assert_noop!(
					Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()),
					Error::<Test>::InsufficientBond,
				);
				assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![11]));

				// 1500 is enough for validator
				assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(3), 500));
				assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![11]));
				assert_ok!(Staking::validate(RuntimeOrigin::signed(3), ValidatorPrefs::default()));

				// Can't unbond anything as validator
				assert_noop!(
					Staking::unbond(RuntimeOrigin::signed(3), 500),
					Error::<Test>::InsufficientBond
				);

				// Once they are a nominator, they can unbond 500
				assert_ok!(Staking::nominate(RuntimeOrigin::signed(3), vec![11]));
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 500));
				assert_noop!(
					Staking::unbond(RuntimeOrigin::signed(3), 500),
					Error::<Test>::InsufficientBond
				);

				// Once they are chilled they can unbond everything
				assert_ok!(Staking::chill(RuntimeOrigin::signed(3)));
				assert_ok!(Staking::unbond(RuntimeOrigin::signed(3), 1000));
			})
	}

	#[test]
	fn chill_other_works() {
		ExtBuilder::default()
			.existential_deposit(100)
			.balance_factor(100)
			.min_nominator_bond(1_000)
			.min_validator_bond(1_500)
			.build_and_execute(|| {
				let initial_validators = Validators::<Test>::count();
				let initial_nominators = Nominators::<Test>::count();
				for i in 0..15 {
					let a = 4 * i;
					let b = 4 * i + 2;
					asset::set_stakeable_balance::<Test>(&a, 100_000);
					asset::set_stakeable_balance::<Test>(&b, 100_000);

					// Nominator
					assert_ok!(Staking::bond(
						RuntimeOrigin::signed(a),
						1000,
						RewardDestination::Stash
					));
					assert_ok!(Staking::nominate(RuntimeOrigin::signed(a), vec![11]));

					// Validator
					assert_ok!(Staking::bond(
						RuntimeOrigin::signed(b),
						1500,
						RewardDestination::Stash
					));
					assert_ok!(Staking::validate(
						RuntimeOrigin::signed(b),
						ValidatorPrefs::default()
					));
					assert_eq!(
						staking_events_since_last_call(),
						vec![
							Event::Bonded { stash: a, amount: 1000 },
							Event::Bonded { stash: b, amount: 1500 },
							Event::ValidatorPrefsSet {
								stash: b,
								prefs: ValidatorPrefs { commission: Zero::zero(), blocked: false }
							}
						]
					);
				}

				// To chill other users, we need to:
				// * Set a minimum bond amount
				// * Set a limit
				// * Set a threshold
				//
				// If any of these are missing, we do not have enough information to allow the
				// `chill_other` to succeed from one user to another.
				//
				// Out of 8 possible cases, only one will allow the use of `chill_other`, which is
				// when all 3 conditions are met.

				// 1. No limits whatsoever
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 2. Change only the minimum bonds.
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Set(1_500),
					ConfigOp::Set(2_000),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Still can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 3. Add nominator/validator count limits, but no other threshold.
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Set(10),
					ConfigOp::Set(10),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Still can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 4. Add chill threshold, but no other limits
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Set(Percent::from_percent(75)),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Still can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 5. Add bond and count limits, but no threshold
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Set(1_500),
					ConfigOp::Set(2_000),
					ConfigOp::Set(10),
					ConfigOp::Set(10),
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Still can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 6. Add bond and threshold limits, but no count limits
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Set(Percent::from_percent(75)),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Still can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 7. Add count limits and a chill threshold, but no bond limits
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Remove,
					ConfigOp::Remove,
					ConfigOp::Set(10),
					ConfigOp::Set(10),
					ConfigOp::Set(Percent::from_percent(75)),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// Still can't chill these users
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 2),
					Error::<Test>::CannotChillOther
				);

				// 8. Add all limits
				assert_ok!(Staking::set_staking_configs(
					RuntimeOrigin::root(),
					ConfigOp::Set(1_500),
					ConfigOp::Set(2_000),
					ConfigOp::Set(10),
					ConfigOp::Set(10),
					ConfigOp::Set(Percent::from_percent(75)),
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
					ConfigOp::Noop,
				));

				// 16 people total because tests start with 2 active one
				assert_eq!(Nominators::<Test>::count(), 15 + initial_nominators);
				assert_eq!(Validators::<Test>::count(), 15 + initial_validators);

				// Users can now be chilled down to 7 people, so we try to remove 9 of them
				// (starting with 16)
				for i in 6..15 {
					let b = 4 * i;
					let d = 4 * i + 2;
					assert_ok!(Staking::chill_other(RuntimeOrigin::signed(1337), b));
					assert_eq!(*staking_events().last().unwrap(), Event::Chilled { stash: b });
					assert_ok!(Staking::chill_other(RuntimeOrigin::signed(1337), d));
					assert_eq!(*staking_events().last().unwrap(), Event::Chilled { stash: d });
				}

				// chill a nominator. Limit is not reached, not chill-able
				assert_eq!(Nominators::<Test>::count(), 7);
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1337), 0),
					Error::<Test>::CannotChillOther
				);
				// chill a validator. Limit is reached, chill-able.
				assert_eq!(Validators::<Test>::count(), 9);
				assert_ok!(Staking::chill_other(RuntimeOrigin::signed(1337), 2));
			})
	}

	#[test]
	fn chill_inactive_works() {
		const NOT_VALIDATOR: AccountId = 10;
		const VALIDATOR: AccountId = 11;

		ExtBuilder::default().build_and_execute(|| {
			let history_depth = HistoryDepth::get();
			let inactive_threshold = history_depth / 2;

			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Set(inactive_threshold)
			));

			Session::roll_until_active_era(history_depth + 1);

			let mut active = BoundedBTreeMap::new();
			active.try_insert(VALIDATOR, 100).unwrap();
			for era in 1..=inactive_threshold {
				ErasRewardPoints::<Test>::insert(
					era,
					EraRewardPoints { total: 100, individual: active.clone() },
				);
			}

			let valid_proof =
				BoundedVec::truncate_from((inactive_threshold + 1..=history_depth).collect());

			assert_noop!(
				Staking::chill_inactive(
					RuntimeOrigin::signed(NOT_VALIDATOR),
					NOT_VALIDATOR,
					valid_proof.clone()
				),
				Error::<Test>::InvalidInactivityProof(
					InvalidInactivityProofError::ValidatorNotExposed
				)
			);

			let mut too_short_proof = valid_proof.clone();
			too_short_proof.remove(0);
			assert_noop!(
				Staking::chill_inactive(
					RuntimeOrigin::signed(NOT_VALIDATOR),
					VALIDATOR,
					too_short_proof
				),
				Error::<Test>::InvalidInactivityProof(InvalidInactivityProofError::InvalidLen)
			);

			let proof_where_one_era_contains_reward_points =
				BoundedVec::truncate_from((2..(history_depth / 2 + 2)).collect());
			assert_noop!(
				Staking::chill_inactive(
					RuntimeOrigin::signed(NOT_VALIDATOR),
					VALIDATOR,
					proof_where_one_era_contains_reward_points
				),
				Error::<Test>::InvalidInactivityProof(InvalidInactivityProofError::ValidatorActive)
			);

			let not_sorted_proof =
				BoundedVec::truncate_from(valid_proof.iter().copied().rev().collect());
			assert_noop!(
				Staking::chill_inactive(
					RuntimeOrigin::signed(NOT_VALIDATOR),
					VALIDATOR,
					not_sorted_proof
				),
				Error::<Test>::InvalidInactivityProof(InvalidInactivityProofError::NotSorted)
			);

			let proof_with_current_era =
				BoundedVec::truncate_from((inactive_threshold + 2..=history_depth + 1).collect());
			assert_noop!(
				Staking::chill_inactive(
					RuntimeOrigin::signed(NOT_VALIDATOR),
					VALIDATOR,
					proof_with_current_era
				),
				Error::<Test>::InvalidInactivityProof(InvalidInactivityProofError::InvalidEra)
			);

			// Era 0 is in the past and the validator is exposed + inactive there, but it falls
			// before the retained window `[active_era - HistoryDepth, active_era)` (here, era 0 <
			// 81 - 80 = 1), so the proof must be rejected on the lower bound.
			let proof_with_too_old_era =
				BoundedVec::truncate_from((0..inactive_threshold).collect());
			assert_noop!(
				Staking::chill_inactive(
					RuntimeOrigin::signed(NOT_VALIDATOR),
					VALIDATOR,
					proof_with_too_old_era
				),
				Error::<Test>::InvalidInactivityProof(InvalidInactivityProofError::InvalidEra)
			);

			assert_ok!(Staking::chill_inactive(
				RuntimeOrigin::signed(NOT_VALIDATOR),
				VALIDATOR,
				valid_proof
			));

			System::assert_last_event(Event::<Test>::Chilled { stash: VALIDATOR }.into());
		});
	}

	#[test]
	fn capped_stakers_works() {
		ExtBuilder::default().build_and_execute(|| {
			let validator_count = Validators::<Test>::count();
			assert_eq!(validator_count, 3);
			let nominator_count = Nominators::<Test>::count();
			assert_eq!(nominator_count, 1);

			// Change the maximums
			let max = 10;
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Set(10),
				ConfigOp::Set(10),
				ConfigOp::Set(max),
				ConfigOp::Set(max),
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop
			));

			// can create `max - validator_count` validators
			let mut some_existing_validator = AccountId::default();
			for i in 0..max - validator_count {
				let (_, controller) = testing_utils::create_stash_controller::<Test>(
					i + 10_000_000,
					100,
					RewardDestination::Stash,
				)
				.unwrap();
				assert_ok!(Staking::validate(
					RuntimeOrigin::signed(controller),
					ValidatorPrefs::default()
				));
				some_existing_validator = controller;
			}

			// but no more
			let (_, last_validator) =
				testing_utils::create_stash_controller::<Test>(1337, 100, RewardDestination::Stash)
					.unwrap();

			assert_noop!(
				Staking::validate(RuntimeOrigin::signed(last_validator), ValidatorPrefs::default()),
				Error::<Test>::TooManyValidators,
			);

			// same with nominators
			let mut some_existing_nominator = AccountId::default();
			for i in 0..max - nominator_count {
				let (_, controller) = testing_utils::create_stash_controller::<Test>(
					i + 20_000_000,
					100,
					RewardDestination::Stash,
				)
				.unwrap();
				assert_ok!(Staking::nominate(RuntimeOrigin::signed(controller), vec![11]));
				some_existing_nominator = controller;
			}

			// one more is too many.
			let (_, last_nominator) = testing_utils::create_stash_controller::<Test>(
				30_000_000,
				100,
				RewardDestination::Stash,
			)
			.unwrap();
			assert_noop!(
				Staking::nominate(RuntimeOrigin::signed(last_nominator), vec![1]),
				Error::<Test>::TooManyNominators
			);

			// Re-nominate works fine
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(some_existing_nominator), vec![11]));
			// Re-validate works fine
			assert_ok!(Staking::validate(
				RuntimeOrigin::signed(some_existing_validator),
				ValidatorPrefs::default()
			));

			// No problem when we set to `None` again
			assert_ok!(Staking::set_staking_configs(
				RuntimeOrigin::root(),
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Remove,
				ConfigOp::Remove,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop,
				ConfigOp::Noop
			));
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(last_nominator), vec![11]));
			assert_ok!(Staking::validate(
				RuntimeOrigin::signed(last_validator),
				ValidatorPrefs::default()
			));
		})
	}
}
