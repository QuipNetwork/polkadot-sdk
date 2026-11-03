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

use crate::{
	reward::EraRewardManager,
	session_rotation::{Eras, Rotator},
	tests::session_mock::{CurrentIndex, Timestamp},
	POT_POOL_SIZE,
};
use codec::Encode;
use frame_support::{
	traits::fungible::{Inspect, Mutate},
	PalletId,
};

use super::*;

#[test]
fn forcing_force_none() {
	ExtBuilder::default().build_and_execute(|| {
		ForceEra::<T>::put(Forcing::ForceNone);

		Session::roll_to_next_session();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::SessionRotated { starting_session: 4, active_era: 1, planned_era: 1 }]
		);

		Session::roll_to_next_session();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::SessionRotated { starting_session: 5, active_era: 1, planned_era: 1 }]
		);

		Session::roll_to_next_session();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::SessionRotated { starting_session: 6, active_era: 1, planned_era: 1 }]
		);

		Session::roll_to_next_session();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 1 }]
		);

		Session::roll_to_next_session();
		assert_eq!(
			staking_events_since_last_call(),
			vec![Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 }]
		);
	});
}

#[test]
fn forcing_no_forcing_default() {
	ExtBuilder::default().build_and_execute(|| {
		// default value, setting it again just for read-ability.
		ForceEra::<T>::put(Forcing::NotForcing);

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
	});
}

#[test]
fn forcing_force_always() {
	ExtBuilder::default()
		.session_per_era(6)
		.no_flush_events()
		.build_and_execute(|| {
			// initial events thus far, without `ForceAlways` set.
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

			// but with it set..
			ForceEra::<T>::put(Forcing::ForceAlways);

			Session::roll_until_active_era(2);
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					// we immediately plan a new era as soon as the first session report comes in
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					// by now it is given to mock session, and is buffered
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 2 },
					// and by now it is activated. Note how the validator payout is less, since the
					// era duration is less. Note that we immediately plan the next era as well.
					Event::EraPaid { era_index: 1, validator_payout: 7500, remainder: 0 },
					Event::SessionRotated { starting_session: 9, active_era: 2, planned_era: 3 }
				]
			);
		});
}

#[test]
fn forcing_force_new() {
	ExtBuilder::default()
		.session_per_era(6)
		.no_flush_events()
		.build_and_execute(|| {
			// initial events thus far, without `ForceAlways` set.
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

			// but with it set..
			ForceEra::<T>::put(Forcing::ForceNew);

			// one era happens quicker
			Session::roll_until_active_era(2);
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					// we immediately plan a new era as soon as the first session report comes in
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					// by now it is given to mock session, and is buffered
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 2 },
					// and by now it is activated. Note how the validator payout is less, since the
					// era duration is less.
					Event::EraPaid { era_index: 1, validator_payout: 7500, remainder: 0 },
					Event::SessionRotated { starting_session: 9, active_era: 2, planned_era: 2 }
				]
			);

			// And the next era goes back to normal.
			Session::roll_until_active_era(3);
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 10, active_era: 2, planned_era: 2 },
					Event::SessionRotated { starting_session: 11, active_era: 2, planned_era: 2 },
					Event::SessionRotated { starting_session: 12, active_era: 2, planned_era: 2 },
					Event::SessionRotated { starting_session: 13, active_era: 2, planned_era: 3 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 14, active_era: 2, planned_era: 3 },
					Event::EraPaid { era_index: 2, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 15, active_era: 3, planned_era: 3 }
				]
			);
		});
}

#[test]
fn activation_timestamp_when_no_planned_era() {
	// maybe not needed, as we have the id check
	ExtBuilder::default().session_per_era(6).build_and_execute(|| {
		Session::roll_until_active_era(2);
		let current_index = CurrentIndex::get();

		// reset events until now.
		let _ = staking_events_since_last_call();

		// GIVEN: no new planned era
		assert_eq!(Rotator::<T>::active_era(), 2);
		assert_eq!(Rotator::<T>::planned_era(), 2);

		// WHEN: send a new activation timestamp (manually).
		<Staking as pallet_staking_async_rc_client::AHStakingInterface>::on_relay_session_report(
			pallet_staking_async_rc_client::SessionReport::new_terminal(
				current_index,
				vec![],
				// sending a timestamp that is in the future with identifier of the next era that
				// is not planned.
				Some((Timestamp::get() + time_per_session(), 3)),
			),
		);

		// THEN: No era rotation should happen, but an error event should be emitted.
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				Event::Unexpected(UnexpectedKind::UnknownValidatorActivation),
				Event::SessionRotated {
					starting_session: current_index + 1,
					active_era: 2,
					planned_era: 2
				}
			]
		);
	});
}

#[test]
#[should_panic]
fn activation_timestamp_when_era_planning_not_complete() {
	// maybe not needed, as we have the id check
	todo!("what if we receive an activation timestamp when the era planning (election) is not complete?");
}

#[test]
fn era_cleanup_history_depth_works_with_prune_era_step_extrinsic() {
	ExtBuilder::default().build_and_execute(|| {
		// Test that era pruning does not happen automatically
		assert_eq!(active_era(), 1);

		Session::roll_until_active_era(HistoryDepth::get() - 1);
		assert!(matches!(
			&staking_events_since_last_call()[..],
			&[
				..,
				Event::SessionRotated { starting_session: 236, active_era: 78, planned_era: 79 },
				Event::EraPaid { era_index: 78, validator_payout: 7500, remainder: 0 },
				Event::SessionRotated { starting_session: 237, active_era: 79, planned_era: 79 }
			]
		));
		// Verify era 78 staker pot has been funded (DAP drips into general pot, staking snapshots).
		let staker_pot_78 = <Test as Config>::RewardPots::pot_account(RewardPot::Era(
			78,
			RewardKind::StakerRewards,
		));
		let ideal_validator_payout = validator_payout_for(time_per_era());
		assert_eq!(Balances::balance(&staker_pot_78), ideal_validator_payout);
		// All eras from 1 to current still present
		assert_ok!(Eras::<T>::era_fully_present(1));
		assert_ok!(Eras::<T>::era_fully_present(2));
		// ..
		assert_ok!(Eras::<T>::era_fully_present(HistoryDepth::get() - 1));

		Session::roll_until_active_era(HistoryDepth::get());
		assert_ok!(Eras::<T>::era_fully_present(1));
		assert_ok!(Eras::<T>::era_fully_present(2));
		// ..
		assert_ok!(Eras::<T>::era_fully_present(HistoryDepth::get()));

		// Eras should NOT be automatically pruned
		Session::roll_until_active_era(HistoryDepth::get() + 1);
		assert_ok!(Eras::<T>::era_fully_present(1));
		assert_ok!(Eras::<T>::era_fully_present(2));
		// ..
		assert_ok!(Eras::<T>::era_fully_present(HistoryDepth::get() + 1));
		assert!(matches!(
			&staking_events_since_last_call()[..],
			&[
				..,
				// NO EraPruned event - pruning is now manual
				Event::EraPaid { era_index: 80, validator_payout: 7500, remainder: 0 },
				Event::SessionRotated { starting_session: 243, active_era: 81, planned_era: 81 }
			]
		));

		// Roll forward more, era 1 is now prunable
		Session::roll_until_active_era(HistoryDepth::get() + 2);
		assert_ok!(Eras::<T>::era_fully_present(1)); // Era 1 still exists!
		assert_ok!(Eras::<T>::era_fully_present(2));
		assert_ok!(Eras::<T>::era_fully_present(3));
		// ..
		assert_ok!(Eras::<T>::era_fully_present(HistoryDepth::get() + 2));
		assert!(matches!(
			&staking_events_since_last_call()[..],
			&[
				..,
				// NO EraPruned event - pruning is now manual
				Event::EraPaid { era_index: 81, validator_payout: 7500, remainder: 0 },
				Event::SessionRotated { starting_session: 246, active_era: 82, planned_era: 82 }
			]
		));
		// Verify eras 79-81 staker pots were funded with expected amount.
		let expected_per_era = validator_payout_for(time_per_era());
		for era in 79..=81 {
			let staker_pot = <Test as Config>::RewardPots::pot_account(RewardPot::Era(
				era,
				RewardKind::StakerRewards,
			));
			assert_eq!(
				Balances::balance(&staker_pot),
				expected_per_era,
				"Era {era} staker pot should have {expected_per_era}"
			);
		}

		// Only old eras (outside pruning window) can be pruned
		// Try to prune era 2 (should fail as it's within the history window)
		assert_noop!(
			Staking::prune_era_step(RuntimeOrigin::signed(99), 2),
			Error::<T>::EraNotPrunable
		);
		// Try to prune the current era
		assert_noop!(
			Staking::prune_era_step(RuntimeOrigin::signed(99), HistoryDepth::get() + 2),
			Error::<T>::EraNotPrunable
		);

		// Verify that we can manually prune era 1 (which is outside history window) and check that
		// we progress through all PruningStep states in the exact order, with storage cleanup
		// verification
		use crate::PruningStep::*;

		// Process each pruning step in the exact order defined by the implementation
		// Each step should clean its specific storage and transition to the next step

		// Process each pruning step, potentially with multiple calls due to item limits
		let steps_order = [
			ErasStakersPaged,
			ErasStakersOverview,
			ErasValidatorPrefs,
			ClaimedRewards,
			ErasValidatorReward,
			ErasRewardPoints,
			SingleEntryCleanups,
			ValidatorSlashInEra,
			ErasValidatorIncentiveWeight,
		];

		let _ = staking_events_since_last_call();
		for expected_step in steps_order.iter() {
			// May need multiple calls for steps with lots of data due to weight limits
			loop {
				let current_state = EraPruningState::<T>::get(1)
					.expect("Era 1 should be marked for pruning at this point");
				assert_eq!(
					current_state, *expected_step,
					"Expected to be in step {:?} but was in {:?}",
					expected_step, current_state
				);

				let result = Staking::prune_era_step(RuntimeOrigin::signed(99), 1);
				assert_ok!(&result);
				let post_info = result.unwrap();

				// When work is actually done (pruning storage), should return Pays::No
				assert_eq!(
					post_info.pays_fee,
					frame_support::dispatch::Pays::No,
					"Should return Pays::No when work is done for step {:?}",
					expected_step
				);

				// Verify weight tracking and limits
				assert!(
					post_info.actual_weight.is_some(),
					"Should report actual weight for {:?}",
					expected_step
				);
				let actual_weight = post_info.actual_weight.unwrap();
				assert!(
					actual_weight.ref_time() > 0,
					"Should report non-zero ref_time for {:?}",
					expected_step
				);
				// No need to validate against limits since we use item-based limiting

				// Check if we've moved to the next step (step completed)
				let new_state = EraPruningState::<T>::get(1).unwrap_or(ErasStakersPaged);
				if new_state != current_state {
					break; // Step completed, move to next
				}
				// Otherwise continue with same step (partial completion due to item limits)
			}

			// Verify the specific storage is cleaned after completing this step
			match expected_step {
				ErasStakersPaged => assert_eq!(
					crate::ErasStakersPaged::<T>::iter_prefix_values((1,)).count(),
					0,
					"{expected_step:?} should be empty after completing step"
				),
				ErasStakersOverview => assert_eq!(
					crate::ErasStakersOverview::<T>::iter_prefix_values(1).count(),
					0,
					"{expected_step:?} should be empty after completing step"
				),
				ErasValidatorPrefs => assert_eq!(
					crate::ErasValidatorPrefs::<T>::iter_prefix_values(1).count(),
					0,
					"{expected_step:?} should be empty after completing step"
				),
				ClaimedRewards => assert_eq!(
					crate::ClaimedRewards::<T>::iter_prefix_values(1).count(),
					0,
					"{expected_step:?} should be empty after completing step"
				),
				ErasValidatorReward => {
					assert!(
						!crate::ErasValidatorReward::<T>::contains_key(1),
						"{expected_step:?} should be empty after completing step"
					);
				},
				ErasRewardPoints => {
					assert!(
						!crate::ErasRewardPoints::<T>::contains_key(1),
						"{expected_step:?} should be empty after completing step"
					);
				},
				SingleEntryCleanups => {
					assert!(
						!crate::ErasTotalStake::<T>::contains_key(1),
						"{expected_step:?} should be empty after completing step"
					);
					assert!(
						!crate::ErasNominatorsSlashable::<T>::contains_key(1),
						"ErasNominatorsSlashable should be empty after completing SingleEntryCleanups step"
					);
					assert!(
						!crate::ErasValidatorIncentiveBudget::<T>::contains_key(1),
						"ErasValidatorIncentiveBudget should be empty after completing SingleEntryCleanups step"
					);
					assert!(
						!crate::ErasSumValidatorIncentiveWeight::<T>::contains_key(1),
						"ErasSumValidatorIncentiveWeight should be empty after completing SingleEntryCleanups step"
					);
				},
				ValidatorSlashInEra => assert_eq!(
					crate::ValidatorSlashInEra::<T>::iter_prefix_values(1).count(),
					0,
					"{expected_step:?} should be empty after completing step"
				),
				ErasValidatorIncentiveWeight => assert_eq!(
					crate::ErasValidatorIncentiveWeight::<T>::iter_prefix_values(1).count(),
					0,
					"{expected_step:?} should be empty after completing step"
				),
			}
		}

		// After final step (ValidatorSlashInEra), the EraPruningState should be removed
		assert!(
			EraPruningState::<T>::get(1).is_none(),
			"EraPruningState should be removed after final step"
		);

		// Should emit exactly one EraPruned event when manual pruning completes
		assert!(matches!(&staking_events_since_last_call()[..], &[Event::EraPruned { index: 1 }]));

		// Attempting to prune again should return an error
		let result = Staking::prune_era_step(RuntimeOrigin::signed(99), 1);
		assert_noop!(result, Error::<T>::EraNotPrunable);

		// Now era 1 should be absent
		assert_ok!(Eras::<T>::era_absent(1));
		// But era 2 should still be present (not automatically pruned)
		assert_ok!(Eras::<T>::era_fully_present(2));

		// Call the extrinsic on an already pruned era (should return error)
		let result = Staking::prune_era_step(RuntimeOrigin::signed(99), 1);
		assert_noop!(result, Error::<T>::EraNotPrunable);
	});
}

#[test]
fn progress_many_eras_with_try_state() {
	// a bit slow, but worthwhile
	ExtBuilder::default().build_and_execute(|| {
		Session::roll_until_active_era_with(
			HistoryDepth::get().max(BondingDuration::get()) + 2,
			|| {
				Staking::do_try_state(System::block_number()).unwrap();
			},
		);
	})
}

mod inflation {
	use super::*;

	#[test]
	fn dap_budget_allocation_determines_staker_rewards() {
		ExtBuilder::default().build_and_execute(|| {
			// 50% of time_per_era() goes to stakers (other half to buffer per
			// mock::default_budget())
			let default_stakers_payout = validator_payout_for(time_per_era());
			assert_eq!(default_stakers_payout, Balance::from(time_per_era()) / 2);

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

			assert_eq!(ErasValidatorReward::<Test>::get(0).unwrap(), default_stakers_payout);
		})
	}
}

#[test]
fn era_pot_drained_after_history_depth() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN: Start at era 2
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		// Verify era-1 staker pot was funded with expected amount.
		let staker_pot_1 =
			<Test as Config>::RewardPots::pot_account(RewardPot::Era(1, RewardKind::StakerRewards));
		let expected_per_era = validator_payout_for(time_per_era());
		assert_eq!(Balances::balance(&staker_pot_1), expected_per_era);

		// era we expect to be drained
		let drained_era = 1;

		// WHEN: Advance past HistoryDepth so era 1 falls out of the active window.
		let target_era = drained_era + HistoryDepth::get() + 1;
		Session::roll_until_active_era(target_era);
		let _ = staking_events_since_last_call();

		// THEN: era-1's pot account holds zero balance but is kept alive (provider
		// retained) so a future era reusing the same slot can snapshot into it.
		let staker_pot = <Test as Config>::RewardPots::pot_account(RewardPot::Era(
			drained_era,
			RewardKind::StakerRewards,
		));

		assert_eq!(Balances::balance(&staker_pot), 0, "Staker pot should have zero balance");
		assert_eq!(
			System::providers(&staker_pot),
			1,
			"Staker pot is kept alive for slot reuse; provider must be retained"
		);
	});
}

#[test]
fn pot_slot_reuse_drain_then_recreate_is_idempotent() {
	// Drain must keep the slot alive, and a subsequent `create()` on a future
	// era sharing the same slot must not double-increment the provider.
	ExtBuilder::default().build_and_execute(|| {
		let era_a = 5;
		let era_b = era_a + POT_POOL_SIZE;

		// GIVEN: era_a's pot is created and funded.
		let pot = EraRewardManager::<Test>::create(era_a, RewardKind::StakerRewards);
		assert_eq!(System::providers(&pot), 1);
		let funded: Balance = 1_000;
		Balances::set_balance(&pot, funded);
		assert_eq!(Balances::balance(&pot), funded);

		// WHEN: era_a's pot is cleaned up past HistoryDepth.
		EraRewardManager::<Test>::cleanup_era(era_a);

		// THEN: balance drained, provider retained (slot kept alive).
		assert_eq!(Balances::balance(&pot), 0);
		assert_eq!(System::providers(&pot), 1, "drain must not release the provider");

		// WHEN: era_b reuses the same slot.
		EraRewardManager::<Test>::create(era_b, RewardKind::StakerRewards);

		// THEN: provider count unchanged (idempotent create).
		assert_eq!(
			System::providers(&pot),
			1,
			"create must not double-increment provider on slot reuse"
		);

		// AND: a fresh snapshot into the reused slot works as if it were new.
		Balances::set_balance(&pot, 2_000);
		assert_eq!(Balances::balance(&pot), 2_000);
	});
}

#[test]
fn era_pot_slots_collide_every_pool_size_eras() {
	// Verifies the production `Seed` provider derives era pots from `(slot, kind)`
	// rather than `(era, kind)`. Asserted on the encoded seed rather than the
	// resulting `AccountId` because the mock's `AccountId = u64` is too narrow
	// to fit the seed and `into_sub_account_truncating` truncates it down to a
	// constant.
	let seed_for = |era: u32, kind: RewardKind| -> Vec<u8> {
		(
			<PalletId as sp_runtime::TypeId>::TYPE_ID,
			DapPalletId::get(),
			RewardPot::Era(crate::pot_slot(era), kind),
		)
			.encode()
	};

	let base_era = 7u32;

	// distinct seeds within a pool window, collision exactly at distance `POT_POOL_SIZE`.
	for kind in [RewardKind::StakerRewards, RewardKind::ValidatorSelfStake] {
		let base_seed = seed_for(base_era, kind);

		for offset in 1..POT_POOL_SIZE {
			assert_ne!(
				seed_for(base_era + offset, kind),
				base_seed,
				"{:?} pot at era +{} must not collide within the pool window",
				kind,
				offset,
			);
		}

		assert_eq!(
			seed_for(base_era + POT_POOL_SIZE, kind),
			base_seed,
			"{:?} pot at era +POT_POOL_SIZE must reuse the base era's slot",
			kind,
		);
	}

	// Within a single slot, different reward kinds must remain distinct.
	assert_ne!(
		seed_for(base_era, RewardKind::StakerRewards),
		seed_for(base_era, RewardKind::ValidatorSelfStake),
		"staker-rewards and incentive pots within the same slot must be distinct",
	);
}

#[test]
fn disable_legacy_minting_era_updates_correctly() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN: DisableMintingGuard is set to 0 in test genesis
		assert_eq!(DisableMintingGuard::<Test>::get(), Some(0));

		// WHEN: Era 1 ends with non-zero reward allocation
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		// THEN: DisableMintingGuard remains at 0
		assert_eq!(DisableMintingGuard::<Test>::get(), Some(0));
	});
}

#[test]
fn disable_legacy_minting_era_write_once_semantics() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN: Clear DisableMintingGuard to simulate pre-migration state
		DisableMintingGuard::<Test>::kill();
		assert_eq!(DisableMintingGuard::<Test>::get(), None);

		// WHEN: First era ends with rewards
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		// THEN: DisableMintingGuard is set to era 1
		assert_eq!(DisableMintingGuard::<Test>::get(), Some(1));

		// WHEN: More eras end
		Session::roll_until_active_era(5);
		let _ = staking_events_since_last_call();

		// THEN: DisableMintingGuard stays at 1 (not updated to higher values)
		assert_eq!(DisableMintingGuard::<Test>::get(), Some(1));
	});
}

#[test]
fn dap_era_with_zero_rewards_still_sets_guard() {
	// A DAP era with nothing to snapshot is still a DAP era. The guard must be set so
	// payout routing for this era uses the DAP path, not legacy minting.
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN: pre-migration state (no guard) and a budget that drips nothing to the
		// staker reward pot.
		DisableMintingGuard::<Test>::kill();
		pallet_dap::BudgetAllocation::<Test>::put(build_budget(&[(buffer_key(), 100)]));
		assert_eq!(DisableMintingGuard::<Test>::get(), None);

		// WHEN: an era ends with zero staker rewards.
		Session::roll_until_active_era(2);
		let _ = staking_events_since_last_call();

		// THEN: guard records this era so future payouts route via DAP.
		assert_eq!(ErasValidatorReward::<Test>::get(1), Some(0));
		assert_eq!(DisableMintingGuard::<Test>::get(), Some(1));
	});
}
