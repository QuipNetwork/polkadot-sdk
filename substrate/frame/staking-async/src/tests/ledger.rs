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
fn paired_account_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_ok!(Staking::bond(RuntimeOrigin::signed(10), 100, RewardDestination::Account(10)));

		assert_eq!(<Bonded<Test>>::get(&10), Some(10));
		assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Controller(10)), Some(10));
		assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(10)), Some(10));

		assert_eq!(<Bonded<Test>>::get(&42), None);
		assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Controller(42)), None);
		assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(42)), None);

		// bond manually stash with different controller. This is deprecated but the migration
		// has not been complete yet (controller: 100, stash: 200)
		assert_ok!(bond_controller_stash(100, 200));
		assert_eq!(<Bonded<Test>>::get(&200), Some(100));
		assert_eq!(
			StakingLedger::<Test>::paired_account(StakingAccount::Controller(100)),
			Some(200)
		);
		assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(200)), Some(100));
	})
}

#[test]
fn get_ledger_works() {
	ExtBuilder::default().build_and_execute(|| {
		// stash does not exist
		assert!(StakingLedger::<Test>::get(StakingAccount::Stash(42)).is_err());

		// bonded and paired
		assert_eq!(<Bonded<Test>>::get(&11), Some(11));

		match StakingLedger::<Test>::get(StakingAccount::Stash(11)) {
			Ok(ledger) => {
				assert_eq!(ledger.controller(), Some(11));
				assert_eq!(ledger.stash, 11);
			},
			Err(_) => panic!("staking ledger must exist"),
		};

		// bond manually stash with different controller. This is deprecated but the migration
		// has not been complete yet (controller: 100, stash: 200)
		assert_ok!(bond_controller_stash(100, 200));
		assert_eq!(<Bonded<Test>>::get(&200), Some(100));

		match StakingLedger::<Test>::get(StakingAccount::Stash(200)) {
			Ok(ledger) => {
				assert_eq!(ledger.controller(), Some(100));
				assert_eq!(ledger.stash, 200);
			},
			Err(_) => panic!("staking ledger must exist"),
		};

		match StakingLedger::<Test>::get(StakingAccount::Controller(100)) {
			Ok(ledger) => {
				assert_eq!(ledger.controller(), Some(100));
				assert_eq!(ledger.stash, 200);
			},
			Err(_) => panic!("staking ledger must exist"),
		};
	})
}

#[test]
fn get_ledger_bad_state_fails() {
	ExtBuilder::default().has_stakers(false).try_state(false).build_and_execute(|| {
		setup_double_bonded_ledgers();

		// Case 1: double bonded but not corrupted:
		// stash 444 has controller 555:
		assert_eq!(Bonded::<Test>::get(444), Some(555));
		assert_eq!(Ledger::<Test>::get(555).unwrap().stash, 444);

		// stash 444 is also a controller of 333:
		assert_eq!(Bonded::<Test>::get(333), Some(444));
		assert_eq!(StakingLedger::<Test>::paired_account(StakingAccount::Stash(333)), Some(444));
		assert_eq!(Ledger::<Test>::get(444).unwrap().stash, 333);

		// although 444 is double bonded (it is a controller and a stash of different ledgers),
		// we can safely retrieve the ledger and mutate it since the correct ledger is
		// returned.
		let ledger_result = StakingLedger::<Test>::get(StakingAccount::Stash(444));
		assert_eq!(ledger_result.unwrap().stash, 444); // correct ledger.

		let ledger_result = StakingLedger::<Test>::get(StakingAccount::Controller(444));
		assert_eq!(ledger_result.unwrap().stash, 333); // correct ledger.

		// fetching ledger 333 by its stash works.
		let ledger_result = StakingLedger::<Test>::get(StakingAccount::Stash(333));
		assert_eq!(ledger_result.unwrap().stash, 333);

		// Case 2: corrupted ledger bonding.
		// in this case, we simulate what happens when fetching a ledger by stash returns a
		// ledger with a different stash. when this happens, we return an error instead of the
		// ledger to prevent ledger mutations.
		let mut ledger = Ledger::<Test>::get(444).unwrap();
		assert_eq!(ledger.stash, 333);
		ledger.stash = 444;
		Ledger::<Test>::insert(444, ledger);

		// now, we are prevented from fetching the ledger by stash from 1. It's associated
		// controller (2) is now bonding a ledger with a different stash (2, not 1).
		assert!(StakingLedger::<Test>::get(StakingAccount::Stash(333)).is_err());
	})
}

#[test]
fn bond_works() {
	ExtBuilder::default().build_and_execute(|| {
		asset::set_stakeable_balance::<T>(&42, 1000);
		assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Stash(42)));
		assert!(<Bonded<Test>>::get(&42).is_none());

		let mut ledger: StakingLedger<Test> = StakingLedger::new(42, 84);
		let reward_dest = RewardDestination::Account(10);

		assert_ok!(ledger.clone().bond(reward_dest));
		assert!(StakingLedger::<Test>::is_bonded(StakingAccount::Stash(42)));
		assert!(<Bonded<Test>>::get(&42).is_some());
		assert_eq!(<Payee<Test>>::get(&42), Some(reward_dest));

		// cannot bond again.
		assert!(ledger.clone().bond(reward_dest).is_err());

		// once bonded, unbonding (or any other update) works as expected.
		ledger.unlocking = bounded_vec![UnlockChunk { era: 42, value: 42 }];
		ledger.active -= 42;
		assert_ok!(ledger.update());
	})
}

#[test]
fn bond_controller_cannot_be_stash_works() {
	ExtBuilder::default().build_and_execute(|| {
		// `create_unique_stash_controller` bonds `ED * (balance_factor / 10).max(1)`. Pass a
		// `balance_factor` large enough that the bonded amount clears `min_chilled_bond` under
		// the default builder. The test's concern is the `AlreadyPaired` path, not the min-bond
		// thresholds.
		let (stash, controller) = testing_utils::create_unique_stash_controller::<Test>(
			0,
			100,
			RewardDestination::Staked,
			false,
		)
		.unwrap();

		assert_eq!(Bonded::<Test>::get(stash), Some(controller));
		assert_eq!(Ledger::<Test>::get(controller).map(|l| l.stash), Some(stash));

		// existing controller should not be able become a stash.
		assert_noop!(
			Staking::bond(RuntimeOrigin::signed(controller), 10, RewardDestination::Staked),
			Error::<Test>::AlreadyPaired,
		);
	})
}

#[test]
fn is_bonded_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Stash(42)));
		assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Controller(42)));

		// adds entry to Bonded without Ledger pair (should not happen).
		<Bonded<Test>>::insert(42, 42);
		assert!(!StakingLedger::<Test>::is_bonded(StakingAccount::Controller(42)));

		assert_eq!(<Bonded<Test>>::get(&11), Some(11));
		assert!(StakingLedger::<Test>::is_bonded(StakingAccount::Stash(11)));
		assert!(StakingLedger::<Test>::is_bonded(StakingAccount::Controller(11)));

		<Bonded<Test>>::remove(42); // ensures try-state checks pass.
	})
}

#[test]
#[allow(deprecated)]
fn set_payee_errors_on_controller_destination() {
	ExtBuilder::default().build_and_execute(|| {
		Payee::<Test>::insert(11, RewardDestination::Staked);
		assert_noop!(
			Staking::set_payee(RuntimeOrigin::signed(11), RewardDestination::Controller),
			Error::<Test>::ControllerDeprecated
		);
		assert_eq!(Payee::<Test>::get(&11), Some(RewardDestination::Staked));
	})
}

#[test]
#[allow(deprecated)]
fn update_payee_migration_works() {
	ExtBuilder::default().build_and_execute(|| {
		// migrate a `Controller` variant to `Account` variant.
		Payee::<Test>::insert(11, RewardDestination::Controller);
		assert_eq!(Payee::<Test>::get(&11), Some(RewardDestination::Controller));
		assert_ok!(Staking::update_payee(RuntimeOrigin::signed(11), 11));
		assert_eq!(Payee::<Test>::get(&11), Some(RewardDestination::Account(11)));

		// Do not migrate a variant if not `Controller`.
		Payee::<Test>::insert(21, RewardDestination::Stash);
		assert_eq!(Payee::<Test>::get(&21), Some(RewardDestination::Stash));
		assert_noop!(
			Staking::update_payee(RuntimeOrigin::signed(11), 21),
			Error::<Test>::NotController
		);
		assert_eq!(Payee::<Test>::get(&21), Some(RewardDestination::Stash));
	})
}

#[test]
fn set_controller_with_bad_state_ok() {
	ExtBuilder::default().has_stakers(false).nominate(false).build_and_execute(|| {
		setup_double_bonded_ledgers();

		// in this case, setting controller works due to the ordering of the calls.
		assert_ok!(Staking::set_controller(RuntimeOrigin::signed(333)));
		assert_ok!(Staking::set_controller(RuntimeOrigin::signed(444)));
		assert_ok!(Staking::set_controller(RuntimeOrigin::signed(555)));
	})
}

#[test]
fn set_controller_with_bad_state_fails() {
	ExtBuilder::default().has_stakers(false).try_state(false).build_and_execute(|| {
		setup_double_bonded_ledgers();

		// setting the controller of ledger associated with stash 555 fails since its stash is a
		// controller of another ledger.
		assert_noop!(Staking::set_controller(RuntimeOrigin::signed(555)), Error::<Test>::BadState);
		assert_noop!(Staking::set_controller(RuntimeOrigin::signed(444)), Error::<Test>::BadState);
		assert_ok!(Staking::set_controller(RuntimeOrigin::signed(333)));
	})
}

mod deprecate_controller_call {
	use super::*;

	#[test]
	fn deprecate_controller_batch_works_full_weight() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			// Given:

			let start = 1001;
			let mut controllers: Vec<_> = vec![];
			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let ctlr: u64 = n.into();
				let stash: u64 = (n + 10000).into();

				Ledger::<Test>::insert(
					ctlr,
					StakingLedger {
						controller: None,
						total: (10 + ctlr).into(),
						active: (10 + ctlr).into(),
						..StakingLedger::default_from(stash)
					},
				);
				Bonded::<Test>::insert(stash, ctlr);
				Payee::<Test>::insert(stash, RewardDestination::Staked);

				controllers.push(ctlr);
			}

			// When:

			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(controllers).unwrap();

			// Only `AdminOrigin` can sign.
			assert_noop!(
				Staking::deprecate_controller_batch(
					RuntimeOrigin::signed(2),
					bounded_controllers.clone()
				),
				BadOrigin
			);

			let result =
				Staking::deprecate_controller_batch(RuntimeOrigin::root(), bounded_controllers);
			assert_ok!(result);
			assert_eq!(
				result.unwrap().actual_weight.unwrap(),
				<Test as Config>::WeightInfo::deprecate_controller_batch(
					<Test as Config>::MaxControllersInDeprecationBatch::get()
				)
			);

			// Then:

			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let ctlr: u64 = n.into();
				let stash: u64 = (n + 10000).into();

				// Ledger no longer keyed by controller.
				assert_eq!(Ledger::<Test>::get(ctlr), None);
				// Bonded now maps to the stash.
				assert_eq!(Bonded::<Test>::get(stash), Some(stash));

				// Ledger is now keyed by stash.
				let ledger_updated = Ledger::<Test>::get(stash).unwrap();
				assert_eq!(ledger_updated.stash, stash);

				// Check `active` and `total` values match the original ledger set by controller.
				assert_eq!(ledger_updated.active, (10 + ctlr).into());
				assert_eq!(ledger_updated.total, (10 + ctlr).into());
			}
		})
	}

	#[test]
	fn deprecate_controller_batch_works_half_weight() {
		ExtBuilder::default().build_and_execute(|| {
			// Given:

			let start = 1001;
			let mut controllers: Vec<_> = vec![];
			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let ctlr: u64 = n.into();

				// Only half of entries are unique pairs.
				let stash: u64 = if n % 2 == 0 { (n + 10000).into() } else { ctlr };

				Ledger::<Test>::insert(
					ctlr,
					StakingLedger { controller: None, ..StakingLedger::default_from(stash) },
				);
				Bonded::<Test>::insert(stash, ctlr);
				Payee::<Test>::insert(stash, RewardDestination::Staked);

				controllers.push(ctlr);
			}

			// When:
			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(controllers.clone()).unwrap();

			let result =
				Staking::deprecate_controller_batch(RuntimeOrigin::root(), bounded_controllers);
			assert_ok!(result);
			assert_eq!(
				result.unwrap().actual_weight.unwrap(),
				<Test as Config>::WeightInfo::deprecate_controller_batch(controllers.len() as u32)
			);

			// Then:

			for n in start..(start + MaxControllersInDeprecationBatch::get()).into() {
				let unique_pair = n % 2 == 0;
				let ctlr: u64 = n.into();
				let stash: u64 = if unique_pair { (n + 10000).into() } else { ctlr };

				// Side effect of migration for unique pair.
				if unique_pair {
					assert_eq!(Ledger::<Test>::get(ctlr), None);
				}
				// Bonded maps to the stash.
				assert_eq!(Bonded::<Test>::get(stash), Some(stash));

				// Ledger is keyed by stash.
				let ledger_updated = Ledger::<Test>::get(stash).unwrap();
				assert_eq!(ledger_updated.stash, stash);
			}
		})
	}

	#[test]
	fn deprecate_controller_batch_skips_unmigrated_controller_payees() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			// Given:

			let stash: u64 = 1000;
			let ctlr: u64 = 1001;

			Ledger::<Test>::insert(
				ctlr,
				StakingLedger { controller: None, ..StakingLedger::default_from(stash) },
			);
			Bonded::<Test>::insert(stash, ctlr);
			#[allow(deprecated)]
			Payee::<Test>::insert(stash, RewardDestination::Controller);

			// When:

			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(vec![ctlr]).unwrap();

			let result =
				Staking::deprecate_controller_batch(RuntimeOrigin::root(), bounded_controllers);
			assert_ok!(result);
			assert_eq!(
				result.unwrap().actual_weight.unwrap(),
				<Test as Config>::WeightInfo::deprecate_controller_batch(1 as u32)
			);

			// Then:

			// Esure deprecation did not happen.
			assert_eq!(Ledger::<Test>::get(ctlr).is_some(), true);

			// Bonded still keyed by controller.
			assert_eq!(Bonded::<Test>::get(stash), Some(ctlr));

			// Ledger is still keyed by controller.
			let ledger_updated = Ledger::<Test>::get(ctlr).unwrap();
			assert_eq!(ledger_updated.stash, stash);
		})
	}

	#[test]
	fn deprecate_controller_batch_with_bad_state_ok() {
		ExtBuilder::default().has_stakers(false).nominate(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// now let's deprecate all the controllers for all the existing ledgers.
			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(vec![333, 444, 555, 777]).unwrap();

			assert_ok!(Staking::deprecate_controller_batch(
				RuntimeOrigin::root(),
				bounded_controllers
			));

			assert_eq!(
				*staking_events().last().unwrap(),
				Event::ControllerBatchDeprecated { failures: 0 }
			);
		})
	}

	#[test]
	fn deprecate_controller_batch_with_bad_state_failures() {
		ExtBuilder::default().has_stakers(false).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// now let's deprecate all the controllers for all the existing ledgers.
			let bounded_controllers: BoundedVec<
				_,
				<Test as Config>::MaxControllersInDeprecationBatch,
			> = BoundedVec::try_from(vec![777, 555, 444, 333]).unwrap();

			assert_ok!(Staking::deprecate_controller_batch(
				RuntimeOrigin::root(),
				bounded_controllers
			));

			assert_eq!(
				*staking_events().last().unwrap(),
				Event::ControllerBatchDeprecated { failures: 2 }
			);
		})
	}
}

mod ledger_recovery {
	use super::*;

	#[test]
	fn inspect_recovery_ledger_simple_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// non corrupted ledger.
			assert_eq!(Staking::inspect_bond_state(&11).unwrap(), LedgerIntegrityState::Ok);

			// non bonded stash.
			assert!(Bonded::<Test>::get(&1111).is_none());
			assert!(Staking::inspect_bond_state(&1111).is_err());

			// double bonded but not corrupted.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
		})
	}

	#[test]
	fn inspect_recovery_ledger_corupted_killed_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			let lock_333_before = asset::staked::<Test>(&333);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(333)
			// (444, 444) -> corrupted and None.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 however is OK.
			assert_eq!(Staking::inspect_bond_state(&444).unwrap(), LedgerIntegrityState::Ok);

			// kill the corrupted ledger that is associated with stash 333.
			assert_ok!(StakingLedger::<Test>::kill(&333));

			// 333 bond is no more but it returns `BadState` because the lock on this stash is
			// still set (see checks below).
			assert_eq!(Staking::inspect_bond_state(&333), Err(Error::<Test>::BadState));
			// now the *other* ledger associated with 444 has been corrupted and killed (None).
			assert_eq!(
				Staking::inspect_bond_state(&444),
				Ok(LedgerIntegrityState::CorruptedKilled)
			);

			// side effects on 333 - ledger, bonded, payee, lock should be completely empty.
			// however, 333 lock remains.
			assert_eq!(asset::staked::<Test>(&333), lock_333_before); // NOK
			assert!(Bonded::<Test>::get(&333).is_none()); // OK
			assert!(Payee::<Test>::get(&333).is_none()); // OK
			assert!(Ledger::<Test>::get(&444).is_none()); // OK

			// side effects on 444 - ledger, bonded, payee, lock should remain be intact.
			// however, 444 lock was removed.
			assert_eq!(asset::staked::<Test>(&444), 0); // NOK
			assert!(Bonded::<Test>::get(&444).is_some()); // OK
			assert!(Payee::<Test>::get(&444).is_some()); // OK
			assert!(Ledger::<Test>::get(&555).is_none()); // NOK

			assert!(Staking::do_try_state(System::block_number()).is_err());
		})
	}

	#[test]
	fn inspect_recovery_ledger_corupted_killed_other_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			let lock_333_before = asset::staked::<Test>(&333);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(444)
			// (333, 444) -> corrupted and None
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 however is OK.
			assert_eq!(Staking::inspect_bond_state(&444).unwrap(), LedgerIntegrityState::Ok);

			// kill the *other* ledger that is double bonded but not corrupted.
			assert_ok!(StakingLedger::<Test>::kill(&444));

			// now 333 is corrupted and None through the *other* ledger being killed.
			assert_eq!(
				Staking::inspect_bond_state(&333).unwrap(),
				LedgerIntegrityState::CorruptedKilled,
			);
			// 444 is cleaned and not a stash anymore; no lock left behind.
			assert_eq!(Ledger::<Test>::get(&444), None);
			assert_eq!(Staking::inspect_bond_state(&444), Err(Error::<Test>::NotStash));

			// side effects on 333 - ledger, bonded, payee, lock should be intact.
			assert_eq!(asset::staked::<Test>(&333), lock_333_before); // OK
			assert_eq!(Bonded::<Test>::get(&333), Some(444)); // OK
			assert!(Payee::<Test>::get(&333).is_some());
			// however, ledger associated with its controller was killed.
			assert!(Ledger::<Test>::get(&444).is_none()); // NOK

			// side effects on 444 - ledger, bonded, payee, lock should be completely removed.
			assert_eq!(asset::staked::<Test>(&444), 0); // OK
			assert!(Bonded::<Test>::get(&444).is_none()); // OK
			assert!(Payee::<Test>::get(&444).is_none()); // OK
			assert!(Ledger::<Test>::get(&555).is_none()); // OK

			assert!(Staking::do_try_state(System::block_number()).is_err());
		})
	}

	#[test]
	fn inspect_recovery_ledger_lock_corrupted_works() {
		ExtBuilder::default().has_stakers(true).try_state(false).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// get into lock corrupted ledger state by bond_extra on a ledger that is double bonded
			// with a corrupted ledger.
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			//  bond_extra(333, 10) -> lock corrupted on 444
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);
			bond_extra_no_checks(&333, 10);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 ledger is not corrupted but locks got out of sync.
			assert_eq!(
				Staking::inspect_bond_state(&444).unwrap(),
				LedgerIntegrityState::LockCorrupted
			);
		})
	}

	// Corrupted ledger restore.
	//
	// * Double bonded and corrupted ledger.
	#[test]
	fn restore_ledger_corrupted_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// get into corrupted and killed ledger state.
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// recover the ledger bonded by 333 stash.
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}

	// Corrupted and killed ledger restore.
	//
	// * Double bonded and corrupted ledger.
	// * Ledger killed by own controller.
	#[test]
	fn restore_ledger_corrupted_killed_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// ledger.total == lock
			let total_444_before_corruption = asset::staked::<Test>(&444);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(333)
			// (444, 444) -> corrupted and None.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// kill the corrupted ledger that is associated with stash 333.
			assert_ok!(StakingLedger::<Test>::kill(&333));

			// 333 bond is no more but it returns `BadState` because the lock on this stash is
			// still set (see checks below).
			assert_eq!(Staking::inspect_bond_state(&333), Err(Error::<Test>::BadState));
			// now the *other* ledger associated with 444 has been corrupted and killed (None).
			assert!(Staking::ledger(StakingAccount::Stash(444)).is_err());

			// try-state should fail.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// recover the ledger bonded by 333 stash.
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));

			// for the try-state checks to pass, we also need to recover the stash 444 which is
			// corrupted too by proxy of kill(333). Currently, both the lock and the ledger of 444
			// have been cleared so we need to provide the new amount to restore the ledger.
			assert_noop!(
				Staking::restore_ledger(RuntimeOrigin::root(), 444, None, None, None),
				Error::<Test>::CannotRestoreLedger
			);

			assert_ok!(Staking::restore_ledger(
				RuntimeOrigin::root(),
				444,
				None,
				Some(total_444_before_corruption),
				None,
			));

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}

	// Corrupted and killed by *other* ledger restore.
	//
	// * Double bonded and corrupted ledger.
	// * Ledger killed by own controller.
	#[test]
	fn restore_ledger_corrupted_killed_other_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// kill(444)
			// (333, 444) -> corrupted and None
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// 333 is corrupted since it's controller is linking 444 ledger.
			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Corrupted);
			// 444 however is OK.
			assert_eq!(Staking::inspect_bond_state(&444).unwrap(), LedgerIntegrityState::Ok);

			// kill the *other* ledger that is double bonded but not corrupted.
			assert_ok!(StakingLedger::<Test>::kill(&444));

			// recover the ledger bonded by 333 stash.
			assert_ok!(Staking::restore_ledger(RuntimeOrigin::root(), 333, None, None, None));

			// 444 does not need recover in this case since it's been killed successfully.
			assert_eq!(Staking::inspect_bond_state(&444), Err(Error::<Test>::NotStash));

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}

	// Corrupted with bond_extra.
	//
	// * Double bonded and corrupted ledger.
	// * Corrupted ledger calls `bond_extra`
	#[test]
	fn restore_ledger_corrupted_bond_extra_works() {
		ExtBuilder::default().has_stakers(true).build_and_execute(|| {
			setup_double_bonded_ledgers();

			let lock_333_before = asset::staked::<Test>(&333);
			let lock_444_before = asset::staked::<Test>(&444);

			// get into corrupted and killed ledger state by killing a corrupted ledger:
			// init state:
			//  (333, 444)
			//  (444, 555)
			// set_controller(444) to 444
			//  (333, 444) -> corrupted
			//  (444, 444)
			// bond_extra(444, 40) -> OK
			// bond_extra(333, 30) -> locks out of sync

			assert_eq!(Staking::inspect_bond_state(&333).unwrap(), LedgerIntegrityState::Ok);
			set_controller_no_checks(&444);

			// now try-state fails.
			assert!(Staking::do_try_state(System::block_number()).is_err());

			// if 444 bonds extra, the locks remain in sync.
			bond_extra_no_checks(&444, 40);
			assert_eq!(asset::staked::<Test>(&333), lock_333_before);
			assert_eq!(asset::staked::<Test>(&444), lock_444_before + 40);

			// however if 333 bonds extra, the wrong lock is updated.
			bond_extra_no_checks(&333, 30);
			assert_eq!(asset::staked::<Test>(&333), lock_444_before + 40 + 30); // not OK
			assert_eq!(asset::staked::<Test>(&444), lock_444_before + 40); // OK

			// recover the ledger bonded by 333 stash. Note that the total/lock needs to be
			// re-written since on-chain data lock has become out of sync.
			assert_ok!(Staking::restore_ledger(
				RuntimeOrigin::root(),
				333,
				None,
				Some(lock_333_before + 30),
				None
			));

			// now recover 444 that although it's not corrupted, its lock and ledger.total are out
			// of sync. in which case, we need to explicitly set the ledger's lock and amount,
			// otherwise the ledger recover will fail.
			assert_noop!(
				Staking::restore_ledger(RuntimeOrigin::root(), 444, None, None, None),
				Error::<Test>::CannotRestoreLedger
			);

			// and enforcing a new ledger lock/total on this non-corrupted ledger will work.
			assert_ok!(Staking::restore_ledger(
				RuntimeOrigin::root(),
				444,
				None,
				Some(lock_444_before + 40),
				None
			));

			// double-check that ledgers got to expected state and bond_extra done during the
			// corrupted state is part of the recovered ledgers.
			let ledger_333 = Bonded::<Test>::get(&333).and_then(Ledger::<Test>::get).unwrap();
			let ledger_444 = Bonded::<Test>::get(&444).and_then(Ledger::<Test>::get).unwrap();

			assert_eq!(ledger_333.total, lock_333_before + 30);
			assert_eq!(asset::staked::<Test>(&333), ledger_333.total);
			assert_eq!(ledger_444.total, lock_444_before + 40);
			assert_eq!(asset::staked::<Test>(&444), ledger_444.total);

			// try-state checks are ok now.
			assert_ok!(Staking::do_try_state(System::block_number()));
		})
	}
}
