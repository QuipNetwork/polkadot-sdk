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

//! Tests for try-state checks.

use super::*;
use frame_support::assert_ok;

#[test]
fn try_state_works_with_uninitialized_pallet() {
	sp_io::TestExternalities::default().execute_with(|| {
		// Verify the pallet is uninitialized
		assert!(ActiveEra::<Test>::get().is_none());
		assert!(CurrentEra::<Test>::get().is_none());
		assert_eq!(Bonded::<Test>::iter().count(), 0);
		assert_eq!(Ledger::<Test>::iter().count(), 0);
		assert_eq!(Validators::<Test>::iter().count(), 0);
		assert_eq!(Nominators::<Test>::iter().count(), 0);

		// Try-state should pass with uninitialized state
		assert_ok!(Staking::do_try_state(System::block_number()));
	});
}

#[test]
fn try_state_detects_inconsistent_active_current_era() {
	ExtBuilder::default().has_stakers(false).build_and_execute(|| {
		// Set only ActiveEra (CurrentEra remains None) - this violates the invariant
		ActiveEra::<Test>::put(ActiveEraInfo { index: 1, start: None });
		CurrentEra::<Test>::kill();

		// Try-state should fail due to inconsistent state
		assert!(Staking::do_try_state(System::block_number()).is_err());

		// Now set only CurrentEra (ActiveEra None) - this also violates the invariant
		ActiveEra::<Test>::kill();
		CurrentEra::<Test>::put(1);

		// Try-state should fail due to inconsistent state
		assert!(Staking::do_try_state(System::block_number()).is_err());

		// Both None should pass
		ActiveEra::<Test>::kill();
		CurrentEra::<Test>::kill();
		assert_ok!(Staking::do_try_state(System::block_number()));

		// Both Some should pass (assuming other invariants are met)
		ActiveEra::<Test>::put(ActiveEraInfo { index: 1, start: None });
		CurrentEra::<Test>::put(1);
		// Need to set up bonded eras for this to pass
		use frame_support::BoundedVec;
		let bonded_eras: BoundedVec<(u32, u32), _> =
			BoundedVec::try_from(vec![(0, 0), (1, 0)]).unwrap();
		BondedEras::<Test>::put(bonded_eras);
		assert_ok!(Staking::do_try_state(System::block_number()));
	});
}

#[test]
fn try_state_bad_exposure() {
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		Session::roll_until_active_era(2);
		assert!(Staking::do_try_state(System::block_number()).is_ok());

		let (validator, mut metadata) = ErasStakersOverview::<T>::iter()
			.take(1)
			.map(|(_era, validator, metadata)| (validator, metadata))
			.collect::<Vec<_>>()
			.pop()
			.unwrap();
		metadata.total += 1;
		ErasStakersOverview::<T>::insert(2, validator, metadata);
		assert!(Staking::do_try_state(System::block_number()).is_err());
	});
}

#[test]
fn last_validator_era_can_be_one_greater_than_active_era() {
	// When the election for the next era has finished but the era is not yet active,
	// `LastValidatorEra` is set to `active_era + 1`.
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		Session::roll_until_active_era(1);
		let era = active_era();

		// Before election, `LastValidatorEra` equals the active era.
		for (validator, _) in ErasStakersOverview::<T>::iter_prefix(era) {
			assert_eq!(LastValidatorEra::<T>::get(&validator), Some(era));
		}

		// Roll session by session until the election for the next era has been stored, i.e.
		// `ErasStakersOverview` for the next era is populated.
		while ErasStakersOverview::<T>::iter_prefix(era + 1).next().is_none() {
			Session::roll_to_next_session();
		}

		// Election for era 2 is stored but era 2 is not yet active.
		assert_eq!(active_era(), 1);
		assert_eq!(current_era(), 2);

		// After election, `LastValidatorEra` is now 1 greater than active era.
		for (validator, _) in ErasStakersOverview::<T>::iter_prefix(era) {
			assert_eq!(LastValidatorEra::<T>::get(&validator), Some(era + 1));
		}
	});
}

#[test]
fn try_state_bad_eras_total_stake() {
	ExtBuilder::default().try_state(false).build_and_execute(|| {
		Session::roll_until_active_era(2);
		assert!(Staking::do_try_state(System::block_number()).is_ok());
		ErasTotalStake::<T>::mutate(2, |s| *s -= 1);
		assert!(Staking::do_try_state(System::block_number()).is_err());
	});
}

#[test]
fn try_state_detects_incentive_weight_mismatch() {
	ExtBuilder::default().build_and_execute(|| {
		// GIVEN: valid incentive state after era rotation.
		setup_incentive_with_budget(45, 5);
		Session::roll_until_active_era(2);

		let era = 2;
		let stored_total = ErasSumValidatorIncentiveWeight::<Test>::get(era);
		assert!(stored_total > 0);

		// WHEN: corrupt the sum to be inconsistent with individual weights.
		ErasSumValidatorIncentiveWeight::<Test>::insert(era, stored_total + 999);

		// THEN: try-state detects the mismatch.
		assert!(Staking::do_try_state(System::block_number()).is_err());

		// Restore valid state so build_and_execute post-check passes.
		ErasSumValidatorIncentiveWeight::<Test>::insert(era, stored_total);
	});
}
