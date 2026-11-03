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

//! Tests to verify the staking functions when `AreNominatorsSlashable` is set to `false`.
//!
//! When nominators are not slashable:
//! - Nominators are NOT slashed when a validator they back commits an offence.
//! - Nominators can unbond and withdraw in 1 era (instead of full `BondingDuration`).
//! - Validators are still slashed and must wait full `BondingDuration` to withdraw.

use super::*;
use sp_staking::StakingUnchecked;

/// When `AreNominatorsSlashable` is false, only validators are slashed, not nominators.
#[test]
fn nominators_are_not_slashed() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.set_nominators_slashable(false)
		.build_and_execute(|| {
			let initial_exposure = Staking::eras_stakers(active_era(), &11);
			assert_eq!(
				initial_exposure,
				Exposure {
					total: 1250,
					own: 1000,
					others: vec![IndividualExposure { who: 101, value: 250 }]
				}
			);

			// staked values before slash
			let nominator_stake = Staking::ledger(101.into()).unwrap().active;
			let nominator_balance = asset::stakeable_balance::<Test>(&101);
			let validator_stake = Staking::ledger(11.into()).unwrap().active;
			let validator_balance = asset::stakeable_balance::<Test>(&11);

			// register a slash for validator 11 with 10%.
			add_slash(11);
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::OffenceReported {
					offence_era: 1,
					validator: 11,
					fraction: Perbill::from_percent(10)
				}]
			);

			// roll one block until slash is applied
			assert_eq!(SlashDeferDuration::get(), 0);
			Session::roll_next();

			// Only validator is slashed, NOT the nominator.
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SlashComputed { offence_era: 1, slash_era: 1, offender: 11, page: 0 },
					// Only validator 11 is slashed, no Event::Slashed for nominator 101
					Event::Slashed { staker: 11, amount: 100 },
				]
			);

			// Nominator's stake and balance remain unchanged.
			assert_eq!(Staking::ledger(101.into()).unwrap().active, nominator_stake);
			assert_eq!(asset::stakeable_balance::<Test>(&101), nominator_balance);

			// Validator's stake is decreased.
			assert!(Staking::ledger(11.into()).unwrap().active < validator_stake);
			assert!(asset::stakeable_balance::<Test>(&11) < validator_balance);

			// Validator was slashed 10% of their own stake (1000 * 10% = 100)
			assert_eq!(Staking::ledger(11.into()).unwrap().active, validator_stake - 100);
			assert_eq!(asset::stakeable_balance::<Test>(&11), validator_balance - 100);
		});
}

/// When `AreNominatorsSlashable` is false, nominators can unbond and withdraw in the next era.
#[test]
fn nominators_can_unbond_in_next_era() {
	ExtBuilder::default().set_nominators_slashable(false).build_and_execute(|| {
		// nominator 101 is bonded
		assert_eq!(
			Staking::ledger(101.into()).unwrap(),
			StakingLedgerInspect {
				stash: 101,
				total: 500,
				active: 500,
				unlocking: Default::default(),
			}
		);

		let era = active_era();
		assert_eq!(era, 1);

		// Nominator unbonds some stake
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(101), 200));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Unbonded {
				stash: 101,
				amount: 200,
				era: active_era() + NominatorFastUnbondDuration::get()
			}]
		);

		// Unlocking should be set to active_era + NominatorFastUnbondDuration (not active_era +
		// BondingDuration)
		let fast_unbond_era = era + NominatorFastUnbondDuration::get();
		assert_eq!(
			Staking::ledger(101.into()).unwrap(),
			StakingLedgerInspect {
				stash: 101,
				total: 500,
				active: 300,
				// Unlocking era is active_era + NominatorFastUnbondDuration (not active_era +
				// BondingDuration)
				unlocking: bounded_vec![UnlockChunk { value: 200, era: fast_unbond_era }],
			}
		);

		// Cannot withdraw yet (still in era 1)
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(101), 0));
		assert_eq!(Staking::ledger(101.into()).unwrap().total, 500); // still 500

		// Roll to the era when fast unbonding completes
		Session::roll_until_active_era(fast_unbond_era);
		assert_eq!(active_era(), fast_unbond_era);

		// Now can withdraw
		let _ = staking_events_since_last_call(); // clear events from era rotation
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(101), 0));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Withdrawn { stash: 101, amount: 200 }]
		);

		assert_eq!(
			Staking::ledger(101.into()).unwrap(),
			StakingLedgerInspect {
				stash: 101,
				total: 300,
				active: 300,
				unlocking: Default::default(),
			}
		);
	});
}

/// When `AreNominatorsSlashable` is false, validators still need to wait full BondingDuration.
#[test]
fn validators_still_have_full_bonding_duration() {
	ExtBuilder::default().set_nominators_slashable(false).build_and_execute(|| {
		// validator 11 is bonded
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 1000,
				unlocking: Default::default(),
			}
		);

		let era = active_era();
		assert_eq!(era, 1);
		let bonding_duration = BondingDuration::get();
		assert_eq!(bonding_duration, 3);

		// Validator unbonds some stake
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 200));
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::Unbonded {
				stash: 11,
				amount: 200,
				era: active_era() + BondingDuration::get()
			}]
		);

		// Unlocking should be set to active_era + BondingDuration (not active_era +
		// NominatorFastUnbondDuration)
		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 1000,
				active: 800,
				// Unlocking era is active_era + BondingDuration = 1 + 3 = 4
				unlocking: bounded_vec![UnlockChunk { value: 200, era: era + bonding_duration }],
			}
		);

		// Cannot withdraw before the full bonding duration expires
		Session::roll_until_active_era(era + 1);
		assert_eq!(active_era(), 2);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(Staking::ledger(11.into()).unwrap().total, 1000); // still locked

		// Cannot withdraw in era 3
		Session::roll_until_active_era(era + 2);
		assert_eq!(active_era(), 3);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(Staking::ledger(11.into()).unwrap().total, 1000); // still locked

		// Can withdraw in era 4 (current_era + bonding_duration)
		Session::roll_until_active_era(era + bonding_duration);
		assert_eq!(active_era(), 4);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));

		assert_eq!(
			Staking::ledger(11.into()).unwrap(),
			StakingLedgerInspect {
				stash: 11,
				total: 800,
				active: 800,
				unlocking: Default::default(),
			}
		);
	});
}

/// When `AreNominatorsSlashable` is false and `SlashDeferDuration` > 0, nominators are still
/// not slashed even when slashes are deferred.
#[test]
fn nominator_not_slashed_with_deferred_slash() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.set_nominators_slashable(false)
		.slash_defer_duration(2)
		.build_and_execute(|| {
			let initial_exposure = Staking::eras_stakers(active_era(), &11);
			assert_eq!(
				initial_exposure,
				Exposure {
					total: 1250,
					own: 1000,
					others: vec![IndividualExposure { who: 101, value: 250 }]
				}
			);

			// staked values before slash
			let nominator_stake = Staking::ledger(101.into()).unwrap().active;
			let nominator_balance = asset::stakeable_balance::<Test>(&101);
			let validator_stake = Staking::ledger(11.into()).unwrap().active;

			// register a slash for validator 11 with 10%.
			add_slash(11);

			// roll one block to process the offence (slash is computed but deferred)
			Session::roll_next();
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::OffenceReported {
						offence_era: 1,
						validator: 11,
						fraction: Perbill::from_percent(10)
					},
					// Slash is computed but deferred (slash_era = offence_era + defer_duration = 1
					// + 2 = 3)
					Event::SlashComputed { offence_era: 1, slash_era: 3, offender: 11, page: 0 },
				]
			);

			// Slash is not applied yet - both stakes unchanged
			assert_eq!(Staking::ledger(101.into()).unwrap().active, nominator_stake);
			assert_eq!(Staking::ledger(11.into()).unwrap().active, validator_stake);

			// Roll to era 3 when slash should be applied
			Session::roll_until_active_era(3);
			// Roll one more block to ensure the slash is applied in on_initialize
			Session::roll_next();

			// Validator is slashed, nominator is NOT slashed
			assert_eq!(Staking::ledger(11.into()).unwrap().active, validator_stake - 100);
			assert_eq!(Staking::ledger(101.into()).unwrap().active, nominator_stake);
			assert_eq!(asset::stakeable_balance::<Test>(&101), nominator_balance);
		});
}

/// Virtual stakers (pool accounts) are also NOT slashed when `AreNominatorsSlashable` is false.
#[test]
fn virtual_staker_not_slashed() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.set_nominators_slashable(false)
		.build_and_execute(|| {
			// Create a virtual staker (like a pool account) that nominates validator 11.
			// Virtual stakers have no actual balance - they are keyless pool accounts.
			let pool_account = 200;
			let payee = 201;
			let pool_stake = 500;
			bond_virtual_nominator(pool_account, payee, pool_stake, vec![11]);

			// Roll to next era so the virtual staker is in the exposure.
			Session::roll_until_active_era(2);

			let exposure = Staking::eras_stakers(active_era(), &11);
			assert_eq!(
				exposure,
				Exposure {
					total: 1000 + pool_stake,
					own: 1000,
					others: vec![IndividualExposure { who: pool_account, value: pool_stake }]
				}
			);

			// Record stakes before slash
			let virtual_staker_stake = Staking::ledger(pool_account.into()).unwrap().active;
			let validator_stake = Staking::ledger(11.into()).unwrap().active;

			// Register a slash for validator 11 with 10%.
			add_slash(11);

			// Roll one block to process the offence.
			assert_eq!(SlashDeferDuration::get(), 0);
			Session::roll_next();

			// Virtual staker's stake remains unchanged (not slashed).
			assert_eq!(Staking::ledger(pool_account.into()).unwrap().active, virtual_staker_stake);

			// Validator is slashed 10% of their own stake.
			assert_eq!(Staking::ledger(11.into()).unwrap().active, validator_stake - 100);
		});
}

/// Virtual stakers (pool accounts) can also unbond using the fast unbond duration when
/// `AreNominatorsSlashable` is false.
#[test]
fn virtual_staker_unbonds_in_one_era() {
	ExtBuilder::default().set_nominators_slashable(false).build_and_execute(|| {
		// Create a virtual staker (like a pool account).
		let pool_account = 200;
		let payee = 201;
		let pool_stake = 500;
		assert_ok!(<Staking as StakingUnchecked>::virtual_bond(&pool_account, pool_stake, &payee));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(pool_account), vec![11]));

		let era = active_era();
		assert_eq!(era, 1);

		// Virtual staker unbonds some stake.
		assert_ok!(<Staking as StakingInterface>::unbond(&pool_account, 200));

		// Unlocking should be set to active_era + NominatorFastUnbondDuration (not active_era +
		// BondingDuration).
		let fast_unbond_era = era + NominatorFastUnbondDuration::get();
		assert_eq!(
			Staking::ledger(pool_account.into()).unwrap(),
			StakingLedgerInspect {
				stash: pool_account,
				total: pool_stake,
				active: pool_stake - 200,
				// Unlocking era is active_era + NominatorFastUnbondDuration (not active_era +
				// BondingDuration)
				unlocking: bounded_vec![UnlockChunk { value: 200, era: fast_unbond_era }],
			}
		);

		// Cannot withdraw yet (still in era 1).
		assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(pool_account, 0));
		assert_eq!(Staking::ledger(pool_account.into()).unwrap().total, pool_stake);

		// Roll to the era when fast unbonding completes.
		Session::roll_until_active_era(fast_unbond_era);
		assert_eq!(active_era(), fast_unbond_era);

		// Now can withdraw.
		assert_ok!(<Staking as StakingInterface>::withdraw_unbonded(pool_account, 0));

		assert_eq!(
			Staking::ledger(pool_account.into()).unwrap(),
			StakingLedgerInspect {
				stash: pool_account,
				total: pool_stake - 200,
				active: pool_stake - 200,
				unlocking: Default::default(),
			}
		);
	});
}

/// Test that `nominator_bonding_duration()` returns the configured fast unbond duration when
/// nominators are not slashable.
///
/// This is the method used by nomination-pools adapter to determine the unbonding period for
/// pool members. When `AreNominatorsSlashable` is false, pool members should also unbond in the
/// fast unbond duration.
#[test]
fn nominator_bonding_duration_returns_one_when_not_slashable() {
	ExtBuilder::default().set_nominators_slashable(false).build_and_execute(|| {
		// When nominators are not slashable, nominator_bonding_duration should return the
		// configured NominatorFastUnbondDuration
		assert_eq!(
			<Staking as StakingInterface>::nominator_bonding_duration(),
			NominatorFastUnbondDuration::get(),
			"nominator_bonding_duration should be NominatorFastUnbondDuration when nominators are not slashable"
		);

		// But bonding_duration (for validators) should still be the full duration
		assert_eq!(
			<Staking as StakingInterface>::bonding_duration(),
			BondingDuration::get(),
			"bonding_duration should still be the full duration"
		);

		// Verify BondingDuration is greater than NominatorFastUnbondDuration to ensure test is meaningful
		assert!(
			BondingDuration::get() > NominatorFastUnbondDuration::get(),
			"BondingDuration should be > NominatorFastUnbondDuration for this test"
		);
	});
}

/// Test that `nominator_bonding_duration()` returns full duration when nominators are slashable.
#[test]
fn nominator_bonding_duration_returns_full_when_slashable() {
	// Default ExtBuilder has nominators_slashable = true
	ExtBuilder::default().build_and_execute(|| {
		// When nominators are slashable, nominator_bonding_duration should equal bonding_duration
		assert_eq!(
			<Staking as StakingInterface>::nominator_bonding_duration(),
			<Staking as StakingInterface>::bonding_duration(),
			"nominator_bonding_duration should equal bonding_duration when nominators are slashable"
		);
	});
}

/// Test that offences from an era where nominators were slashable continue to slash nominators
/// even after the global `AreNominatorsSlashable` is set to false.
///
/// This verifies the era-specific slashing behavior: the rules at the time of the offence apply.
#[test]
fn offence_from_slashable_era_slashes_nominators_even_after_setting_changes() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.build_and_execute(|| {
			// Era 1: nominators are slashable (default)
			assert!(AreNominatorsSlashable::<Test>::get());
			assert_eq!(active_era(), 1);

			// Advance to era 2 to ensure ErasNominatorsSlashable is set for era 1
			Session::roll_until_active_era(2);

			// Verify era 1 was recorded as nominators-slashable
			assert!(
				ErasNominatorsSlashable::<Test>::get(1).unwrap_or(true),
				"Era 1 should have nominators slashable"
			);

			let nominator_stake_before = Staking::ledger(101.into()).unwrap().active;
			let validator_stake_before = Staking::ledger(11.into()).unwrap().active;

			// Now change the global setting to false BEFORE the offence is processed
			AreNominatorsSlashable::<Test>::put(false);

			// Report an offence from era 1 (when nominators WERE slashable)
			add_slash_in_era(11, 1, Perbill::from_percent(10));

			// Roll one block to process the offence
			Session::roll_next();

			// Both validator AND nominator should be slashed because the offence
			// occurred in era 1 when nominators were slashable
			let validator_stake_after = Staking::ledger(11.into()).unwrap().active;
			let nominator_stake_after = Staking::ledger(101.into()).unwrap().active;

			assert!(validator_stake_after < validator_stake_before, "Validator should be slashed");
			assert!(
				nominator_stake_after < nominator_stake_before,
				"Nominator should be slashed because the offence was in a slashable era"
			);
		});
}

/// Test that offences from an era where nominators were NOT slashable do not slash nominators
/// even if the global setting later changes to true.
///
/// This verifies the era-specific slashing behavior in the opposite direction.
#[test]
fn offence_from_non_slashable_era_does_not_slash_nominators_even_after_setting_changes() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.set_nominators_slashable(false)
		.build_and_execute(|| {
			// Era 1: nominators are NOT slashable
			assert!(!AreNominatorsSlashable::<Test>::get());
			assert_eq!(active_era(), 1);

			// Advance to era 2
			Session::roll_until_active_era(2);

			// Verify era 1 was recorded as nominators NOT slashable
			assert!(
				!ErasNominatorsSlashable::<Test>::get(1).unwrap_or(true),
				"Era 1 should have nominators NOT slashable"
			);

			let nominator_stake_before = Staking::ledger(101.into()).unwrap().active;
			let validator_stake_before = Staking::ledger(11.into()).unwrap().active;

			// Now change the global setting to true BEFORE the offence is processed
			AreNominatorsSlashable::<Test>::put(true);

			// Report an offence from era 1 (when nominators were NOT slashable)
			add_slash_in_era(11, 1, Perbill::from_percent(10));

			// Roll one block to process the offence
			Session::roll_next();

			// Only validator should be slashed, NOT nominator, because the offence
			// occurred in era 1 when nominators were NOT slashable
			let validator_stake_after = Staking::ledger(11.into()).unwrap().active;
			let nominator_stake_after = Staking::ledger(101.into()).unwrap().active;

			assert!(validator_stake_after < validator_stake_before, "Validator should be slashed");
			assert_eq!(
				nominator_stake_after, nominator_stake_before,
				"Nominator should NOT be slashed because the offence was in a non-slashable era"
			);
		});
}

/// Test that when nominators slashable setting changes mid-era, offences are processed
/// based on the era they occurred in, not the current setting.
#[test]
fn mixed_era_offences_processed_based_on_era_specific_setting() {
	ExtBuilder::default()
		.validator_count(4)
		.set_status(41, StakerStatus::Validator)
		.build_and_execute(|| {
			// Era 1: nominators are slashable (default)
			assert!(AreNominatorsSlashable::<Test>::get());
			assert_eq!(active_era(), 1);

			// Advance to era 2
			Session::roll_until_active_era(2);
			// Era 2 is also slashable

			// Change setting to false for era 3+
			AreNominatorsSlashable::<Test>::put(false);

			// Advance to era 3
			Session::roll_until_active_era(3);

			// Verify era-specific settings
			assert!(
				ErasNominatorsSlashable::<Test>::get(1).unwrap_or(true),
				"Era 1 should be slashable"
			);
			assert!(
				ErasNominatorsSlashable::<Test>::get(2).unwrap_or(true),
				"Era 2 should be slashable"
			);
			assert!(
				!ErasNominatorsSlashable::<Test>::get(3).unwrap_or(true),
				"Era 3 should NOT be slashable"
			);

			let nominator_stake_before = Staking::ledger(101.into()).unwrap().active;

			// Report offence from era 2 (slashable) - nominator should be slashed
			add_slash_in_era(11, 2, Perbill::from_percent(5));
			Session::roll_next();

			let nominator_stake_after_era2_slash = Staking::ledger(101.into()).unwrap().active;
			assert!(
				nominator_stake_after_era2_slash < nominator_stake_before,
				"Nominator should be slashed for era 2 offence"
			);

			// Report offence from era 3 (NOT slashable) - nominator should NOT be slashed
			add_slash_in_era(11, 3, Perbill::from_percent(5));
			Session::roll_next();

			let nominator_stake_after_era3_slash = Staking::ledger(101.into()).unwrap().active;
			assert_eq!(
				nominator_stake_after_era3_slash, nominator_stake_after_era2_slash,
				"Nominator should NOT be slashed for era 3 offence"
			);
		});
}

/// Test that validators who switch to nominator role must still use full bonding duration.
///
/// This test verifies that a validator cannot avoid slash in following scenario:
/// 1. A validator commits a slashable offence in era N
/// 2. The validator switches to nominator role (calling `nominate()`)
/// 3. Unbond with fast `NominatorFastUnbondDuration` and withdraw before the slash is applied
#[test]
fn validator_cannot_switch_to_nominator_to_avoid_slashing() {
	ExtBuilder::default()
		.set_nominators_slashable(false)
		.slash_defer_duration(2) // Defer slashing by 2 eras
		.build_and_execute(|| {
			let alice = 11;
			// Alice is a validator in era 1
			assert!(Validators::<Test>::contains_key(&alice));
			assert!(!Nominators::<Test>::contains_key(&alice));
			assert_eq!(active_era(), 1);

			let validator_stake_before = Staking::ledger(alice.into()).unwrap().active;
			assert_eq!(validator_stake_before, 1000);

			// Step 1: Alice commits a slashable offence in era 1 (10% slash)
			add_slash(alice);
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::OffenceReported {
					offence_era: 1,
					validator: alice,
					fraction: Perbill::from_percent(10)
				}]
			);

			// Process the offence - it will be computed and deferred to era 3 (1 + 2)
			Session::roll_next();
			assert_eq!(
				staking_events_since_last_call(),
				vec![Event::SlashComputed {
					offence_era: 1,
					slash_era: 3,
					offender: alice,
					page: 0
				}]
			);

			// Step 2: Alice switches to nominator role
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(alice), vec![21]));

			// Verify Alice is now a nominator (no longer a validator)
			assert!(!Validators::<Test>::contains_key(&alice));
			assert!(Nominators::<Test>::contains_key(&alice));

			// Step 3: Alice (now a nominator) unbonds partially. We leave `active` exactly at
			// `MinNominatorBond` (= ED + 1 under the default builder) so the unbond passes the
			// per-role guard.
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(alice), 998));
			assert_eq!(
				staking_events_since_last_call(),
				[Event::Unbonded {
					stash: alice,
					amount: 998,
					era: active_era() + BondingDuration::get()
				}]
			);

			// Alice should still be a nominator
			assert!(Nominators::<Test>::contains_key(&alice));

			// Calculate expected unlock eras:
			// - Fast unbond: current_era (1) + NominatorFastUnbondDuration (2) = 3
			// - Full unbond: current_era (1) + BondingDuration (3) = 4
			let fast_unbond_era = 1 + NominatorFastUnbondDuration::get();
			let validator_unbond_era = 1 + BondingDuration::get();

			assert_eq!(NominatorFastUnbondDuration::get(), 2);
			assert_eq!(BondingDuration::get(), 3);
			assert_eq!(fast_unbond_era, 3);
			assert_eq!(validator_unbond_era, 4);

			// Alice must use full bonding duration despite being a nominator now,
			// because she was a validator in era 1 (within BondingDuration of active era).
			// This prevents her from withdrawing before the slash is applied.
			assert_eq!(
				Staking::ledger(alice.into()).unwrap(),
				StakingLedgerInspect {
					stash: alice,
					total: 1000,
					active: 2,
					unlocking: bounded_vec![UnlockChunk { value: 998, era: validator_unbond_era }],
				}
			);

			// Step 4: Try to withdraw in era 3 (when fast unbond would have completed)
			Session::roll_until_active_era(3);
			let _ = staking_events_since_last_call();
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(alice), 0));

			// Verify no withdrawal occurred - funds are still locked
			assert_eq!(staking_events_since_last_call(), []);
			assert_eq!(
				Staking::ledger(alice.into()).unwrap().total,
				1000 // initial stake
			);

			// Step 5: Slash is applied (happens in the next block)
			Session::roll_next();

			// Verify slash event and that Alice is slashed properly (10% of 1000 = 100)
			assert_eq!(
				staking_events_since_last_call(),
				[Event::Slashed { staker: alice, amount: 100 }]
			);

			let ledger_after_slash = Staking::ledger(alice.into()).unwrap();
			assert_eq_error_rate!(
				ledger_after_slash.total,
				900, // Expected: 1000 - 10% = 900
				1    // Allow small error margin due to rounding
			);

			// Step 6: Wait until era 4 when the full bonding duration expires
			Session::roll_until_active_era(4);
			let _ = staking_events_since_last_call();

			// Now Alice can withdraw - the full bonding duration has elapsed
			assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(alice), 0));

			// Alice was slashed (10% of 1000 = 100). The slash is proportional across `active`
			// and the unlocking chunk: with `active = 2` and `unlocking = 998` it takes 1 from
			// active and 99 from the chunk (rounded up). The chunk becomes 899 and that's what
			// `withdraw_unbonded` releases here; `active = 1` remains in the ledger, so the
			// stash is NOT reaped (kept above ED).
			assert_eq!(
				staking_events_since_last_call(),
				[Event::Withdrawn { stash: alice, amount: 899 }]
			);

			let ledger_after_withdraw = Staking::ledger(alice.into()).expect("ledger remains");
			assert_eq!(ledger_after_withdraw.active, 1);
			assert_eq!(ledger_after_withdraw.total, 1);
			assert!(ledger_after_withdraw.unlocking.is_empty());
		});
}
