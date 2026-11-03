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

//! Staking pallet benchmarking.

use super::*;
use crate::{
	asset,
	session_rotation::{Eras, Rotator},
	Pallet as Staking,
};
use alloc::collections::BTreeMap;
pub use frame_benchmarking::{
	impl_benchmark_test_suite, v2::*, whitelist_account, whitelisted_caller, BenchmarkError,
};
use frame_election_provider_support::SortedListProvider;
use frame_support::{
	assert_ok,
	pallet_prelude::*,
	storage::bounded_vec::BoundedVec,
	traits::{fungible::Inspect, TryCollect},
};
use frame_system::RawOrigin;
use pallet_staking_async_rc_client as rc_client;
use sp_runtime::{
	traits::{Bounded, One, StaticLookup, Zero},
	Perbill, Percent, Saturating,
};
use sp_staking::currency_to_vote::CurrencyToVote;
use testing_utils::*;

const SEED: u32 = 0;

// This function clears all existing validators and nominators from the set, and generates one new
// validator being nominated by n nominators, and returns the validator stash account and the
// nominators' stash and controller. It also starts plans a new era with this new stakers, and
// returns the planned era index.
pub(crate) fn create_validator_with_nominators<T: Config>(
	n: u32,
	upper_bound: u32,
	dead_controller: bool,
	unique_controller: bool,
	destination: RewardDestination<T::AccountId>,
) -> Result<(T::AccountId, Vec<(T::AccountId, T::AccountId)>, EraIndex), &'static str> {
	// TODO: this can be replaced with `testing_utils` version?
	// Clean up any existing state.
	clear_validators_and_nominators::<T>();

	// Disable legacy minting so benchmarks always exercise the reward-pot path.
	DisableMintingGuard::<T>::put(0);
	let mut points_total = 0;
	let mut points_individual = Vec::new();

	let (v_stash, v_controller) = if unique_controller {
		create_unique_stash_controller::<T>(0, 100, destination.clone(), false)?
	} else {
		create_stash_controller::<T>(0, 100, destination.clone())?
	};

	let validator_prefs =
		ValidatorPrefs { commission: Perbill::from_percent(50), ..Default::default() };
	Staking::<T>::validate(RawOrigin::Signed(v_controller).into(), validator_prefs)?;
	let stash_lookup = T::Lookup::unlookup(v_stash.clone());

	points_total += 10;
	points_individual.push((v_stash.clone(), 10));

	let original_nominator_count = Nominators::<T>::count();
	let mut nominators = Vec::new();

	// Give the validator n nominators, but keep total users in the system the same.
	for i in 0..upper_bound {
		let (n_stash, n_controller) = if !dead_controller {
			create_stash_controller::<T>(u32::MAX - i, 100, destination.clone())?
		} else {
			create_unique_stash_controller::<T>(u32::MAX - i, 100, destination.clone(), true)?
		};
		if i < n {
			Staking::<T>::nominate(
				RawOrigin::Signed(n_controller.clone()).into(),
				vec![stash_lookup.clone()],
			)?;
			nominators.push((n_stash, n_controller));
		}
	}

	ValidatorCount::<T>::put(1);

	// Start a new Era
	let new_validators = Rotator::<T>::legacy_insta_plan_era();
	let planned_era = CurrentEra::<T>::get().unwrap_or_default();

	assert_eq!(new_validators.len(), 1, "New validators is not 1");
	assert_eq!(new_validators[0], v_stash, "Our validator was not selected");
	assert_ne!(Validators::<T>::count(), 0, "New validators count wrong");
	assert_eq!(
		Nominators::<T>::count(),
		original_nominator_count + nominators.len() as u32,
		"New nominators count wrong"
	);

	// Give Era Points
	let reward = EraRewardPoints::<T> {
		total: points_total,
		individual: points_individual.into_iter().try_collect()?,
	};

	ErasRewardPoints::<T>::insert(planned_era, reward);

	// Create reward pool
	let total_payout = asset::existential_deposit::<T>()
		.saturating_mul(upper_bound.into())
		.saturating_mul(1000u32.into());
	<ErasValidatorReward<T>>::insert(planned_era, total_payout);

	// Create and fund the era reward pot so payout_stakers can transfer from it.
	let era_pot =
		crate::reward::EraRewardManager::<T>::create(planned_era, RewardKind::StakerRewards);
	let _ = asset::mint_creating::<T>(&era_pot, total_payout);

	// Set up validator incentive so payout benchmarks include the incentive transfer cost.
	OptimumSelfStake::<T>::put(BalanceOf::<T>::from(30_000u64));
	HardCapSelfStake::<T>::put(BalanceOf::<T>::from(100_000u64));
	SelfStakeSlopeFactor::<T>::put(Perbill::from_percent(50));

	let incentive_payout = total_payout / 10u32.into(); // 10% of total as incentive budget
	let incentive_pot =
		crate::reward::EraRewardManager::<T>::create(planned_era, RewardKind::ValidatorSelfStake);
	let _ = asset::mint_creating::<T>(&incentive_pot, incentive_payout);
	ErasValidatorIncentiveBudget::<T>::insert(planned_era, incentive_payout);

	// Single-validator benchmark setup: sum == this validator's weight.
	let incentive_weight = BalanceOf::<T>::from(100u64);
	ErasValidatorIncentiveWeight::<T>::insert(planned_era, &v_stash, incentive_weight);
	ErasSumValidatorIncentiveWeight::<T>::insert(planned_era, incentive_weight);
	// Populate the weighted-points denominator so the (default) weighted-points payout path
	// finds a non-zero share and exercises the incentive transfer. Single validator with
	// `validator_points` points: sum == weight × points.
	let validator_points = BalanceOf::<T>::from(10u64);
	ErasSumWeightedPoints::<T>::insert(
		planned_era,
		incentive_weight.saturating_mul(validator_points),
	);

	Ok((v_stash, nominators, planned_era))
}

struct ListScenario<T: Config> {
	/// Stash that is expected to be moved.
	origin_stash1: T::AccountId,
	/// Controller of the Stash that is expected to be moved.
	origin_controller1: T::AccountId,
	dest_weight: BalanceOf<T>,
}

impl<T: Config> ListScenario<T> {
	/// An expensive scenario for bags-list implementation:
	///
	/// - the node to be updated (r) is the head of a bag that has at least one other node. The bag
	///   itself will need to be read and written to update its head. The node pointed to by r.next
	///   will need to be read and written as it will need to have its prev pointer updated. Note
	///   that there are two other worst case scenarios for bag removal: 1) the node is a tail and
	///   2) the node is a middle node with prev and next; all scenarios end up with the same number
	///   of storage reads and writes.
	///
	/// - the destination bag has at least one node, which will need its next pointer updated.
	///
	/// NOTE: while this scenario specifically targets a worst case for the bags-list, it should
	/// also elicit a worst case for other known `VoterList` implementations; although
	/// this may not be true against unknown `VoterList` implementations.
	fn new(origin_weight: BalanceOf<T>, is_increase: bool) -> Result<Self, &'static str> {
		ensure!(!origin_weight.is_zero(), "origin weight must be greater than 0");

		// burn the entire issuance.
		let i = asset::burn::<T>(asset::total_issuance::<T>());
		core::mem::forget(i);

		// make sure `account("random_validator", 0, SEED)` is a validator
		let validator_account = account("random_validator", 0, SEED);
		let validator_stake = asset::existential_deposit::<T>() * 1000u32.into();
		asset::set_stakeable_balance::<T>(&validator_account, validator_stake);
		assert_ok!(Staking::<T>::bond(
			RawOrigin::Signed(validator_account.clone()).into(),
			validator_stake / 2u32.into(),
			RewardDestination::Staked
		));
		assert_ok!(Staking::<T>::validate(
			RawOrigin::Signed(validator_account.clone()).into(),
			Default::default()
		));

		// create accounts with the origin weight
		let (origin_stash1, origin_controller1) = create_stash_controller_with_balance::<T>(
			USER_SEED + 2,
			origin_weight,
			RewardDestination::Staked,
		)?;
		Staking::<T>::nominate(
			RawOrigin::Signed(origin_controller1.clone()).into(),
			// NOTE: these don't really need to be validators.
			vec![T::Lookup::unlookup(validator_account.clone())],
		)?;

		let (_origin_stash2, origin_controller2) = create_stash_controller_with_balance::<T>(
			USER_SEED + 3,
			origin_weight,
			RewardDestination::Staked,
		)?;
		Staking::<T>::nominate(
			RawOrigin::Signed(origin_controller2).into(),
			vec![T::Lookup::unlookup(validator_account.clone())],
		)?;

		// find a destination weight that will trigger the worst case scenario
		let dest_weight_as_vote =
			T::VoterList::score_update_worst_case(&origin_stash1, is_increase);

		let total_issuance = asset::total_issuance::<T>();

		let dest_weight =
			T::CurrencyToVote::to_currency(dest_weight_as_vote as u128, total_issuance);

		// create an account with the worst case destination weight
		let (_dest_stash1, dest_controller1) = create_stash_controller_with_balance::<T>(
			USER_SEED + 1,
			dest_weight,
			RewardDestination::Staked,
		)?;
		Staking::<T>::nominate(
			RawOrigin::Signed(dest_controller1).into(),
			vec![T::Lookup::unlookup(validator_account)],
		)?;

		Ok(ListScenario { origin_stash1, origin_controller1, dest_weight })
	}
}

const USER_SEED: u32 = 999666;

#[benchmarks]
mod benchmarks {
	use super::*;
	use alloc::format;

	#[benchmark]
	fn bond() {
		let stash = create_funded_user::<T>("stash", USER_SEED, 100);
		let reward_destination = RewardDestination::Staked;
		let amount = asset::existential_deposit::<T>() * 10u32.into();
		whitelist_account!(stash);

		#[extrinsic_call]
		_(RawOrigin::Signed(stash.clone()), amount, reward_destination);

		assert!(Bonded::<T>::contains_key(stash.clone()));
		assert!(Ledger::<T>::contains_key(stash));
	}

	#[benchmark]
	fn bond_extra() -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup the worst case list scenario.

		// the weight the nominator will start at.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;

		let max_additional = scenario.dest_weight - origin_weight;

		let stash = scenario.origin_stash1.clone();
		let controller = scenario.origin_controller1;
		let original_bonded: BalanceOf<T> = Ledger::<T>::get(&controller)
			.map(|l| l.active)
			.ok_or("ledger not created after")?;

		let _ = asset::mint_into_existing::<T>(
			&stash,
			max_additional + asset::existential_deposit::<T>(),
		)
		.unwrap();

		whitelist_account!(stash);

		#[extrinsic_call]
		_(RawOrigin::Signed(stash), max_additional);

		let ledger = Ledger::<T>::get(&controller).ok_or("ledger not created after")?;
		let new_bonded: BalanceOf<T> = ledger.active;
		assert!(original_bonded < new_bonded);

		Ok(())
	}

	#[benchmark]
	fn unbond() -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		// the weight the nominator will start at. The value used here is expected to be
		// significantly higher than the first position in a list (e.g. the first bag threshold).
		let origin_weight = BalanceOf::<T>::try_from(952_994_955_240_703u128)
			.map_err(|_| "balance expected to be a u128")
			.unwrap();
		let scenario = ListScenario::<T>::new(origin_weight, false)?;

		let controller = scenario.origin_controller1.clone();
		let amount = origin_weight - scenario.dest_weight;
		let ledger = Ledger::<T>::get(&controller).ok_or("ledger not created before")?;
		let original_bonded: BalanceOf<T> = ledger.active;

		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller.clone()), amount);

		let ledger = Ledger::<T>::get(&controller).ok_or("ledger not created after")?;
		let new_bonded: BalanceOf<T> = ledger.active;
		assert!(original_bonded > new_bonded);

		Ok(())
	}

	#[benchmark]
	// Withdraw only updates the ledger
	fn withdraw_unbonded_update() -> Result<(), BenchmarkError> {
		let (_, controller) = create_stash_controller::<T>(0, 100, RewardDestination::Staked)?;
		let amount = asset::existential_deposit::<T>() * 5u32.into(); // Half of total
		Staking::<T>::unbond(RawOrigin::Signed(controller.clone()).into(), amount)?;
		set_active_era::<T>(EraIndex::max_value());
		let ledger = Ledger::<T>::get(&controller).ok_or("ledger not created before")?;
		let original_total: BalanceOf<T> = ledger.total;
		whitelist_account!(controller);

		#[extrinsic_call]
		withdraw_unbonded(RawOrigin::Signed(controller.clone()), 0);

		let ledger = Ledger::<T>::get(&controller).ok_or("ledger not created after")?;
		let new_total: BalanceOf<T> = ledger.total;
		assert!(original_total > new_total);

		Ok(())
	}

	#[benchmark]
	// Worst case scenario, everything is removed after the bonding duration
	fn withdraw_unbonded_kill() -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup a worst case list scenario. Note that we don't care about the setup of the
		// destination position because we are doing a removal from the list but no insert.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;
		let controller = scenario.origin_controller1.clone();
		let stash = scenario.origin_stash1;
		assert!(T::VoterList::contains(&stash));

		let ed = asset::existential_deposit::<T>();
		let mut ledger = Ledger::<T>::get(&controller).unwrap();
		ledger.active = ed - One::one();
		Ledger::<T>::insert(&controller, ledger);
		set_active_era::<T>(EraIndex::max_value());

		whitelist_account!(controller);

		#[extrinsic_call]
		withdraw_unbonded(RawOrigin::Signed(controller.clone()), 0);

		assert!(!Ledger::<T>::contains_key(controller));
		assert!(!T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn validate() -> Result<(), BenchmarkError> {
		let (stash, controller) = create_stash_controller::<T>(
			MaxNominationsOf::<T>::get() - 1,
			100,
			RewardDestination::Staked,
		)?;
		// because it is chilled.
		assert!(!T::VoterList::contains(&stash));

		let prefs = ValidatorPrefs::default();
		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller), prefs);

		assert!(Validators::<T>::contains_key(&stash));
		assert!(T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn kick(
		// scenario: we want to kick `k` nominators from nominating us (we are a validator).
		// we'll assume that `k` is under 128 for the purposes of determining the slope.
		// each nominator should have `T::MaxNominations::get()` validators nominated, and our
		// validator should be somewhere in there.
		k: Linear<1, 128>,
	) -> Result<(), BenchmarkError> {
		// these are the other validators; there are `T::MaxNominations::get() - 1` of them, so
		// there are a total of `T::MaxNominations::get()` validators in the system.
		let rest_of_validators =
			create_validators_with_seed::<T>(MaxNominationsOf::<T>::get() - 1, 100, 415)?;

		// this is the validator that will be kicking.
		let (stash, controller) = create_stash_controller::<T>(
			MaxNominationsOf::<T>::get() - 1,
			100,
			RewardDestination::Staked,
		)?;
		let stash_lookup = T::Lookup::unlookup(stash.clone());

		// they start validating.
		Staking::<T>::validate(RawOrigin::Signed(controller.clone()).into(), Default::default())?;

		// we now create the nominators. there will be `k` of them; each will nominate all
		// validators. we will then kick each of the `k` nominators from the main validator.
		let mut nominator_stashes = Vec::with_capacity(k as usize);
		for i in 0..k {
			// create a nominator stash.
			let (n_stash, n_controller) = create_stash_controller::<T>(
				MaxNominationsOf::<T>::get() + i,
				100,
				RewardDestination::Staked,
			)?;

			// bake the nominations; we first clone them from the rest of the validators.
			let mut nominations = rest_of_validators.clone();
			// then insert "our" validator somewhere in there (we vary it) to avoid accidental
			// optimisations/pessimisations.
			nominations.insert(i as usize % (nominations.len() + 1), stash_lookup.clone());
			// then we nominate.
			Staking::<T>::nominate(RawOrigin::Signed(n_controller.clone()).into(), nominations)?;

			nominator_stashes.push(n_stash);
		}

		// all nominators now should be nominating our validator...
		for n in nominator_stashes.iter() {
			assert!(Nominators::<T>::get(n).unwrap().targets.contains(&stash));
		}

		// we need the unlookuped version of the nominator stash for the kick.
		let kicks = nominator_stashes
			.iter()
			.map(|n| T::Lookup::unlookup(n.clone()))
			.collect::<Vec<_>>();

		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller), kicks);

		// all nominators now should *not* be nominating our validator...
		for n in nominator_stashes.iter() {
			assert!(!Nominators::<T>::get(n).unwrap().targets.contains(&stash));
		}

		Ok(())
	}

	#[benchmark]
	// Worst case scenario, T::MaxNominations::get()
	fn nominate(n: Linear<1, { MaxNominationsOf::<T>::get() }>) -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup a worst case list scenario. Note we don't care about the destination position,
		// because we are just doing an insert into the origin position.
		ListScenario::<T>::new(origin_weight, true)?;
		let (stash, controller) = create_stash_controller_with_balance::<T>(
			SEED + MaxNominationsOf::<T>::get() + 1, /* make sure the account does not conflict
			                                          * with others */
			origin_weight,
			RewardDestination::Staked,
		)
		.unwrap();

		assert!(!Nominators::<T>::contains_key(&stash));
		assert!(!T::VoterList::contains(&stash));

		let validators = create_validators::<T>(n, 100).unwrap();
		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller), validators);

		assert!(Nominators::<T>::contains_key(&stash));
		assert!(T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn chill() -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup a worst case list scenario. Note that we don't care about the setup of the
		// destination position because we are doing a removal from the list but no insert.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;
		let controller = scenario.origin_controller1.clone();
		let stash = scenario.origin_stash1;
		assert!(T::VoterList::contains(&stash));

		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller));

		assert!(!T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn set_payee() -> Result<(), BenchmarkError> {
		let (stash, controller) =
			create_stash_controller::<T>(USER_SEED, 100, RewardDestination::Staked)?;
		assert_eq!(Payee::<T>::get(&stash), Some(RewardDestination::Staked));
		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller.clone()), RewardDestination::Account(controller.clone()));

		assert_eq!(Payee::<T>::get(&stash), Some(RewardDestination::Account(controller)));

		Ok(())
	}

	#[benchmark]
	fn update_payee() -> Result<(), BenchmarkError> {
		let (stash, controller) =
			create_stash_controller::<T>(USER_SEED, 100, RewardDestination::Staked)?;
		Payee::<T>::insert(&stash, {
			#[allow(deprecated)]
			RewardDestination::Controller
		});
		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller.clone()), controller.clone());

		assert_eq!(Payee::<T>::get(&stash), Some(RewardDestination::Account(controller)));

		Ok(())
	}

	#[benchmark]
	fn set_controller() -> Result<(), BenchmarkError> {
		let (stash, ctlr) =
			create_unique_stash_controller::<T>(9000, 100, RewardDestination::Staked, false)?;
		// ensure `ctlr` is the currently stored controller.
		assert!(!Ledger::<T>::contains_key(&stash));
		assert!(Ledger::<T>::contains_key(&ctlr));
		assert_eq!(Bonded::<T>::get(&stash), Some(ctlr.clone()));

		whitelist_account!(stash);

		#[extrinsic_call]
		_(RawOrigin::Signed(stash.clone()));

		assert!(Ledger::<T>::contains_key(&stash));

		Ok(())
	}

	#[benchmark]
	fn set_validator_count() {
		let validator_count = T::MaxValidatorSet::get() - 1;

		#[extrinsic_call]
		_(RawOrigin::Root, validator_count);

		assert_eq!(ValidatorCount::<T>::get(), validator_count);
	}

	#[benchmark]
	fn force_no_eras() {
		#[extrinsic_call]
		_(RawOrigin::Root);

		assert_eq!(ForceEra::<T>::get(), Forcing::ForceNone);
	}

	#[benchmark]
	fn force_new_era() {
		#[extrinsic_call]
		_(RawOrigin::Root);

		assert_eq!(ForceEra::<T>::get(), Forcing::ForceNew);
	}

	#[benchmark]
	fn force_new_era_always() {
		#[extrinsic_call]
		_(RawOrigin::Root);

		assert_eq!(ForceEra::<T>::get(), Forcing::ForceAlways);
	}

	#[benchmark]
	fn deprecate_controller_batch(
		// We pass a dynamic number of controllers to the benchmark, up to
		// `MaxControllersInDeprecationBatch`.
		u: Linear<0, { T::MaxControllersInDeprecationBatch::get() }>,
	) -> Result<(), BenchmarkError> {
		let mut controllers: Vec<_> = vec![];
		let mut stashes: Vec<_> = vec![];
		for i in 0..u as u32 {
			let (stash, controller) =
				create_unique_stash_controller::<T>(i, 100, RewardDestination::Staked, false)?;
			controllers.push(controller);
			stashes.push(stash);
		}
		let bounded_controllers: BoundedVec<_, T::MaxControllersInDeprecationBatch> =
			BoundedVec::try_from(controllers.clone()).unwrap();

		#[extrinsic_call]
		_(RawOrigin::Root, bounded_controllers);

		for i in 0..u as u32 {
			let stash = &stashes[i as usize];
			let controller = &controllers[i as usize];
			// Ledger no longer keyed by controller.
			assert_eq!(Ledger::<T>::get(controller), None);
			// Bonded now maps to the stash.
			assert_eq!(Bonded::<T>::get(stash), Some(stash.clone()));
			// Ledger is now keyed by stash.
			assert_eq!(Ledger::<T>::get(stash).unwrap().stash, *stash);
		}

		Ok(())
	}

	#[benchmark]
	fn force_unstake() -> Result<(), BenchmarkError> {
		// Clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup a worst case list scenario. Note that we don't care about the setup of the
		// destination position because we are doing a removal from the list but no insert.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;
		let controller = scenario.origin_controller1.clone();
		let stash = scenario.origin_stash1;
		assert!(T::VoterList::contains(&stash));

		#[extrinsic_call]
		_(RawOrigin::Root, stash.clone(), 0);

		assert!(!Ledger::<T>::contains_key(&controller));
		assert!(!T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn cancel_deferred_slash(s: Linear<1, { T::MaxValidatorSet::get() }>) {
		let era = EraIndex::one();

		// Create validators and insert slashes
		let validators: Vec<_> = (0..s)
			.map(|i| {
				let validator: T::AccountId = account("validator", i, SEED);

				// Insert slash for this validator
				let slash_key = (validator.clone(), Perbill::from_percent(10), 0);
				let unapplied_slash = UnappliedSlash::<T> {
					validator: validator.clone(),
					own: Zero::zero(),
					others: WeakBoundedVec::default(),
					reporter: Default::default(),
					payout: Zero::zero(),
				};
				UnappliedSlashes::<T>::insert(era, slash_key, unapplied_slash);

				validator
			})
			.collect();

		// Convert validators to tuples with 10% slash fraction (matching the slashes created above)
		let validator_slashes: Vec<_> =
			validators.into_iter().map(|v| (v, Perbill::from_percent(10))).collect();

		#[extrinsic_call]
		_(RawOrigin::Root, era, validator_slashes.clone());

		// Ensure cancelled slashes are stored correctly
		let cancelled_slashes = CancelledSlashes::<T>::get(era);
		assert_eq!(cancelled_slashes.len(), s as usize);
	}

	#[benchmark]
	fn payout_stakers_alive_staked(
		n: Linear<0, { T::MaxExposurePageSize::get() as u32 }>,
	) -> Result<(), BenchmarkError> {
		let (validator, nominators, current_era) = create_validator_with_nominators::<T>(
			n,
			T::MaxExposurePageSize::get() as u32,
			false,
			true,
			RewardDestination::Staked,
		)?;

		// set the commission for this particular era as well.
		<ErasValidatorPrefs<T>>::insert(
			current_era,
			validator.clone(),
			Validators::<T>::get(&validator),
		);

		let caller = whitelisted_caller();
		let balance_before = asset::stakeable_balance::<T>(&validator);
		// Incentive pot is funded in the setup; track it to ensure the worst-case payout
		// actually performs the validator-incentive transfer (and so the benchmark weight
		// covers it).
		let incentive_pot = crate::reward::EraRewardManager::<T>::create(
			current_era,
			RewardKind::ValidatorSelfStake,
		);
		let incentive_pot_before = asset::stakeable_balance::<T>(&incentive_pot);
		let mut nominator_balances_before = Vec::new();
		for (stash, _) in &nominators {
			let balance = asset::stakeable_balance::<T>(stash);
			nominator_balances_before.push(balance);
		}

		#[extrinsic_call]
		payout_stakers(RawOrigin::Signed(caller), validator.clone(), current_era);

		let balance_after = asset::stakeable_balance::<T>(&validator);
		ensure!(
			balance_before < balance_after,
			"Balance of validator stash should have increased after payout.",
		);
		ensure!(
			asset::stakeable_balance::<T>(&incentive_pot) < incentive_pot_before,
			"Incentive pot should have decreased: the validator-incentive transfer must run \
			 so its cost is captured by this benchmark.",
		);
		for ((stash, _), balance_before) in nominators.iter().zip(nominator_balances_before.iter())
		{
			let balance_after = asset::stakeable_balance::<T>(stash);
			ensure!(
				balance_before < &balance_after,
				"Balance of nominator stash should have increased after payout.",
			);
		}

		Ok(())
	}

	#[benchmark]
	fn rebond(l: Linear<1, { T::MaxUnlockingChunks::get() as u32 }>) -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Pallet::<T>::min_nominator_bond()
			// we use 100 to play friendly with the list threshold values in the mock
			.max(100u32.into());

		// setup a worst case list scenario.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;
		let dest_weight = scenario.dest_weight;

		// rebond an amount that will give the user dest_weight
		let rebond_amount = dest_weight - origin_weight;

		// spread that amount to rebond across `l` unlocking chunks,
		let value = rebond_amount / l.into();
		// if `value` is zero, we need a greater delta between dest <=> origin weight
		assert_ne!(value, Zero::zero());
		// so the sum of unlocking chunks puts voter into the dest bag.
		assert!(value * l.into() + origin_weight > origin_weight);
		assert!(value * l.into() + origin_weight <= dest_weight);
		let unlock_chunk = UnlockChunk::<BalanceOf<T>> { value, era: EraIndex::zero() };

		let controller = scenario.origin_controller1;
		let mut staking_ledger = Ledger::<T>::get(controller.clone()).unwrap();

		for _ in 0..l {
			staking_ledger.unlocking.try_push(unlock_chunk.clone()).unwrap()
		}
		Ledger::<T>::insert(controller.clone(), staking_ledger.clone());
		let original_bonded: BalanceOf<T> = staking_ledger.active;

		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller.clone()), rebond_amount);

		let ledger = Ledger::<T>::get(&controller).ok_or("ledger not created after")?;
		let new_bonded: BalanceOf<T> = ledger.active;
		assert!(original_bonded < new_bonded);

		Ok(())
	}

	#[benchmark]
	fn reap_stash() -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup a worst case list scenario. Note that we don't care about the setup of the
		// destination position because we are doing a removal from the list but no insert.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;
		let controller = scenario.origin_controller1.clone();
		let stash = scenario.origin_stash1;

		let l =
			StakingLedger::<T>::new(stash.clone(), asset::existential_deposit::<T>() - One::one());
		Ledger::<T>::insert(&controller, l);

		assert!(Bonded::<T>::contains_key(&stash));
		assert!(T::VoterList::contains(&stash));

		whitelist_account!(controller);

		#[extrinsic_call]
		_(RawOrigin::Signed(controller), stash.clone(), 0);

		assert!(!Bonded::<T>::contains_key(&stash));
		assert!(!T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn set_staking_configs_all_set() {
		#[extrinsic_call]
		set_staking_configs(
			RawOrigin::Root,
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(u32::MAX),
			ConfigOp::Set(u32::MAX),
			ConfigOp::Set(Percent::max_value()),
			ConfigOp::Set(Perbill::max_value()),
			ConfigOp::Set(Percent::max_value()),
			ConfigOp::Set(false),
			ConfigOp::Set(10),
		);

		assert_eq!(MinNominatorBond::<T>::get(), BalanceOf::<T>::max_value());
		assert_eq!(MinValidatorBond::<T>::get(), BalanceOf::<T>::max_value());
		assert_eq!(MaxNominatorsCount::<T>::get(), Some(u32::MAX));
		assert_eq!(MaxValidatorsCount::<T>::get(), Some(u32::MAX));
		assert_eq!(ChillThreshold::<T>::get(), Some(Percent::from_percent(100)));
		assert_eq!(MinCommission::<T>::get(), Perbill::from_percent(100));
		assert_eq!(MaxStakedRewards::<T>::get(), Some(Percent::from_percent(100)));
		assert_eq!(AreNominatorsSlashable::<T>::get(), false);
		assert_eq!(ChillInactiveThreshold::<T>::get(), 10);
	}

	#[benchmark]
	fn set_staking_configs_all_remove() {
		#[extrinsic_call]
		set_staking_configs(
			RawOrigin::Root,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
			ConfigOp::Remove,
		);

		assert!(!MinNominatorBond::<T>::exists());
		assert!(!MinValidatorBond::<T>::exists());
		assert!(!MaxNominatorsCount::<T>::exists());
		assert!(!MaxValidatorsCount::<T>::exists());
		assert!(!ChillThreshold::<T>::exists());
		assert!(!MinCommission::<T>::exists());
		assert!(!MaxStakedRewards::<T>::exists());
		assert!(!AreNominatorsSlashable::<T>::exists());
	}

	#[benchmark]
	fn chill_other() -> Result<(), BenchmarkError> {
		// clean up any existing state.
		clear_validators_and_nominators::<T>();

		let origin_weight = Staking::<T>::min_nominator_bond();

		// setup a worst case list scenario. Note that we don't care about the setup of the
		// destination position because we are doing a removal from the list but no insert.
		let scenario = ListScenario::<T>::new(origin_weight, true)?;
		let stash = scenario.origin_stash1;
		assert!(T::VoterList::contains(&stash));

		Staking::<T>::set_staking_configs(
			RawOrigin::Root.into(),
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(0),
			ConfigOp::Set(0),
			ConfigOp::Set(Percent::from_percent(0)),
			ConfigOp::Set(Zero::zero()),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Noop,
		)?;

		let caller = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), stash.clone());

		assert!(!T::VoterList::contains(&stash));

		Ok(())
	}

	#[benchmark]
	fn force_apply_min_commission() -> Result<(), BenchmarkError> {
		// Clean up any existing state
		clear_validators_and_nominators::<T>();

		// Create a validator with a commission of 50%. `balance_factor = 100` gives a bonded
		// amount of `ED * 10`, above `min_chilled_bond` under the mock defaults.
		let (stash, controller) = create_stash_controller::<T>(1, 100, RewardDestination::Staked)?;
		let validator_prefs =
			ValidatorPrefs { commission: Perbill::from_percent(50), ..Default::default() };
		Staking::<T>::validate(RawOrigin::Signed(controller).into(), validator_prefs)?;

		// Sanity check
		assert_eq!(
			Validators::<T>::get(&stash),
			ValidatorPrefs { commission: Perbill::from_percent(50), ..Default::default() }
		);

		// Set the min commission to 75%
		MinCommission::<T>::set(Perbill::from_percent(75));
		let caller = whitelisted_caller();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), stash.clone());

		// The validators commission has been bumped to 75%
		assert_eq!(
			Validators::<T>::get(&stash),
			ValidatorPrefs { commission: Perbill::from_percent(75), ..Default::default() }
		);

		Ok(())
	}

	#[benchmark]
	fn set_min_commission() {
		let min_commission = Perbill::max_value();

		#[extrinsic_call]
		_(RawOrigin::Root, min_commission);

		assert_eq!(MinCommission::<T>::get(), Perbill::from_percent(100));
	}

	#[benchmark]
	fn set_max_commission() {
		let max_commission = Perbill::max_value();

		#[extrinsic_call]
		_(RawOrigin::Root, max_commission);

		assert_eq!(MaxCommission::<T>::get(), Perbill::from_percent(100));
	}

	#[benchmark]
	fn set_validator_self_stake_incentive_config() {
		#[extrinsic_call]
		_(
			RawOrigin::Root,
			ConfigOp::Set(30_000u32.into()),
			ConfigOp::Set(100_000u32.into()),
			ConfigOp::Set(Perbill::from_percent(50)),
		);

		assert_eq!(OptimumSelfStake::<T>::get(), 30_000u32.into());
		assert_eq!(HardCapSelfStake::<T>::get(), 100_000u32.into());
		assert_eq!(SelfStakeSlopeFactor::<T>::get(), Perbill::from_percent(50));
	}

	#[benchmark]
	fn restore_ledger() -> Result<(), BenchmarkError> {
		let (stash, controller) = create_stash_controller::<T>(0, 100, RewardDestination::Staked)?;
		// corrupt ledger.
		Ledger::<T>::remove(controller);

		#[extrinsic_call]
		_(RawOrigin::Root, stash.clone(), None, None, None);

		assert_eq!(Staking::<T>::inspect_bond_state(&stash), Ok(LedgerIntegrityState::Ok));

		Ok(())
	}

	#[benchmark]
	fn migrate_currency() -> Result<(), BenchmarkError> {
		let (stash, _ctrl) =
			create_stash_controller::<T>(USER_SEED, 100, RewardDestination::Staked)?;
		let stake = asset::staked::<T>(&stash);
		migrate_to_old_currency::<T>(stash.clone());
		// no holds
		assert!(asset::staked::<T>(&stash).is_zero());
		whitelist_account!(stash);

		#[extrinsic_call]
		_(RawOrigin::Signed(stash.clone()), stash.clone());

		assert_eq!(asset::staked::<T>(&stash), stake);
		Ok(())
	}

	#[benchmark]
	fn apply_slash(
		n: Linear<0, { T::MaxExposurePageSize::get() as u32 }>,
	) -> Result<(), BenchmarkError> {
		let era = EraIndex::one();
		ActiveEra::<T>::put(ActiveEraInfo { index: era, start: None });
		let (validator, nominators, _current_era) = create_validator_with_nominators::<T>(
			T::MaxExposurePageSize::get() as u32,
			T::MaxExposurePageSize::get() as u32,
			false,
			true,
			RewardDestination::Staked,
		)?;
		let slash_fraction = Perbill::from_percent(10);
		let page_index = 0;
		let slashed_balance = BalanceOf::<T>::from(10u32);

		let slash_key = (validator.clone(), slash_fraction, page_index);
		// Only include `n` nominators in the slash (to benchmark variable nominator counts)
		let slashed_nominators = nominators
			.iter()
			.take(n as usize)
			.map(|(nom, _)| (nom.clone(), slashed_balance))
			.collect::<Vec<_>>();

		let unapplied_slash = UnappliedSlash::<T> {
			validator: validator.clone(),
			own: slashed_balance,
			others: WeakBoundedVec::force_from(slashed_nominators, None),
			reporter: Default::default(),
			payout: Zero::zero(),
		};

		// Insert an unapplied slash to be processed.
		UnappliedSlashes::<T>::insert(era, slash_key.clone(), unapplied_slash);

		#[extrinsic_call]
		_(RawOrigin::Signed(validator.clone()), era, slash_key.clone());

		// Ensure the slash has been applied and removed.
		assert!(UnappliedSlashes::<T>::get(era, &slash_key).is_none());

		Ok(())
	}

	#[benchmark]
	fn process_offence_queue() -> Result<(), BenchmarkError> {
		// in tests, it is likely that `SlashDeferDuration` is zero and this will also insta-apply
		// the slash. Remove this just in case.
		#[cfg(test)]
		crate::mock::SlashDeferDuration::set(77);

		// create at least one validator with a full page of exposure, as per `MaxExposurePageSize`.
		let all_validators = crate::testing_utils::create_validators_with_nominators_for_era::<T>(
			// we create more validators, but all of the nominators will back the first one
			ValidatorCount::<T>::get().max(1),
			// create two full exposure pages
			2 * T::MaxExposurePageSize::get(),
			16,
			false,
			Some(1),
		)?;
		let offender =
			T::Lookup::lookup(all_validators.first().cloned().expect("must exist")).unwrap();

		// plan an era with this set
		let _new_validators = Rotator::<T>::legacy_insta_plan_era();
		// activate the previous one
		Rotator::<T>::start_era(
			crate::ActiveEraInfo { index: Rotator::<T>::planned_era() - 1, start: Some(1) },
			42, // start session index doesn't really matter,
			2,  // timestamp doesn't really matter
		);

		// ensure our offender has at least a full exposure page
		let offender_exposure =
			Eras::<T>::get_full_exposure(Rotator::<T>::planned_era(), &offender);
		ensure!(
			offender_exposure.others.len() as u32 >= T::MaxExposurePageSize::get(),
			"exposure not created"
		);

		// create an offence for this validator
		let slash_session = 42;
		let offences = vec![rc_client::Offence {
			offender: offender.clone(),
			reporters: Default::default(),
			slash_fraction: Perbill::from_percent(50),
		}];
		<crate::Pallet<T> as rc_client::AHStakingInterface>::on_new_offences(
			slash_session,
			offences,
		);

		// ensure offence is submitted
		ensure!(
			ValidatorSlashInEra::<T>::contains_key(Rotator::<T>::active_era(), offender),
			"offence not submitted"
		);
		ensure!(
			OffenceQueueEras::<T>::get().unwrap_or_default() == vec![Rotator::<T>::active_era()],
			"offence should be queued"
		);

		#[block]
		{
			slashing::process_offence::<T>();
		}

		ensure!(OffenceQueueEras::<T>::get().is_none(), "offence should not be queued");

		Ok(())
	}

	#[benchmark]
	fn rc_on_offence(
		v: Linear<2, { T::MaxValidatorSet::get() / 2 }>,
	) -> Result<(), BenchmarkError> {
		let initial_era = Rotator::<T>::planned_era();
		let _ = crate::testing_utils::create_validators_with_nominators_for_era::<T>(
			2 * v,
			// number of nominators is irrelevant here, so we hardcode these
			1000,
			16,
			false,
			None,
		)?;

		// plan new era
		let new_validators = Rotator::<T>::legacy_insta_plan_era();
		ensure!(Rotator::<T>::planned_era() == initial_era + 1, "era should be incremented");
		// activate the previous one
		Rotator::<T>::start_era(
			crate::ActiveEraInfo { index: initial_era, start: Some(1) },
			42, // start session index doesn't really matter,
			2,  // timestamp doesn't really matter
		);

		// this is needed in the slashing code, and is a sign that `initial_era + 1` is planned!
		ensure!(Rotator::<T>::active_era_start_session_index() == 42, "BondedEra not set");

		// slash the first half of the validators
		let to_slash_count = new_validators.len() / 2;
		let to_slash = new_validators.into_iter().take(to_slash_count).collect::<Vec<_>>();
		let one_slashed = to_slash.first().cloned().unwrap();
		let offences = to_slash
			.into_iter()
			.map(|offender| rc_client::Offence {
				offender,
				reporters: Default::default(),
				slash_fraction: Perbill::from_percent(50),
			})
			.collect::<Vec<_>>();
		let slash_session = 42;

		// has not pending slash for these guys now
		ensure!(
			!ValidatorSlashInEra::<T>::contains_key(initial_era + 1, &one_slashed),
			"offence submitted???"
		);

		#[block]
		{
			<crate::Pallet<T> as rc_client::AHStakingInterface>::on_new_offences(
				slash_session,
				offences,
			);
		}

		// ensure offence is recorded
		ensure!(
			ValidatorSlashInEra::<T>::contains_key(initial_era + 1, one_slashed),
			"offence not submitted"
		);

		Ok(())
	}

	#[benchmark]
	fn rc_on_session_report(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let initial_planned_era = Rotator::<T>::planned_era();
		let initial_active_era = Rotator::<T>::active_era();

		// create a small, arbitrary number of stakers. This is just for sanity of the era planning,
		// numbers don't matter.
		crate::testing_utils::create_validators_with_nominators_for_era::<T>(
			10, 50, 2, false, None,
		)?;

		// plan new era
		let _new_validators = Rotator::<T>::legacy_insta_plan_era();
		ensure!(
			CurrentEra::<T>::get().unwrap() == initial_planned_era + 1,
			"era should be incremented"
		);

		//  receive a session report with timestamp that actives the previous one.
		// `v` is the number of active validators in the report — on Polkadot bounded by
		// `MaxValidatorSet` since each must hold at least `MinValidatorBond` (~10k DOT).
		let validator_points = (0..v)
			.map(|i| (account::<T::AccountId>("random", i, SEED), i))
			.collect::<Vec<_>>();

		// Populate ErasValidatorIncentiveWeight for every account in the report so the
		// incremental-denominator accumulation inside `reward_active_era` performs the
		// worst-case lookup-and-add per validator.
		let active_era = Rotator::<T>::active_era();
		for (who, _) in &validator_points {
			ErasValidatorIncentiveWeight::<T>::insert(
				active_era,
				who,
				BalanceOf::<T>::from(100u64),
			);
		}

		let activation_timestamp = Some((1u64, initial_planned_era + 1));
		let report = rc_client::SessionReport {
			end_index: 42,
			leftover: false,
			validator_points,
			activation_timestamp,
		};

		#[block]
		{
			<crate::Pallet<T> as rc_client::AHStakingInterface>::on_relay_session_report(report);
		}

		ensure!(Rotator::<T>::active_era() == initial_active_era + 1, "active era not bumped");
		Ok(())
	}

	// Helper function to set up era data for pruning benchmarks
	fn setup_era_for_pruning<T: Config>(v: u32) -> EraIndex {
		let validators = v;
		let era = 7;

		// Set active era to make era 7 prunable
		// Era is prunable if: era <= active_era - history_depth - 1
		let history_depth = T::HistoryDepth::get();
		let active_era = era + history_depth + 1;
		crate::ActiveEra::<T>::put(crate::ActiveEraInfo { index: active_era, start: Some(0) });

		let max_total_nominators_per_validator =
			<T::ElectionProvider as ElectionProvider>::MaxBackersPerWinnerFinal::get();
		let exposed_nominators_per_validator = max_total_nominators_per_validator / validators;

		let pages: WeakBoundedVec<_, _> = (0..crate::ClaimedRewardsBound::<T>::get())
			.collect::<Vec<_>>()
			.try_into()
			.unwrap();

		// 33% slashed — realistic worst-case under BFT assumptions.
		let slashed_validators = validators / 3;

		let mut reward_points_individual = BTreeMap::new();
		let mut total_incentive_weight = BalanceOf::<T>::zero();

		for i in 0..validators {
			let validator = account::<T::AccountId>("validator", i, SEED);

			// ValidatorPrefs
			ErasValidatorPrefs::<T>::insert(era, validator.clone(), ValidatorPrefs::default());

			// ClaimedRewards
			ClaimedRewards::<T>::insert(era, validator.clone(), pages.clone());

			// ErasStakersPaged + ErasStakersOverview
			let exposure = sp_staking::Exposure::<T::AccountId, BalanceOf<T>> {
				own: T::Currency::minimum_balance(),
				total: T::Currency::minimum_balance() *
					(exposed_nominators_per_validator + 1).into(),
				others: (0..exposed_nominators_per_validator)
					.map(|n| {
						let nominator = account::<T::AccountId>("nominator", n, SEED);
						IndividualExposure { who: nominator, value: T::Currency::minimum_balance() }
					})
					.collect::<Vec<_>>(),
			};
			Eras::<T>::upsert_exposure(era, &validator, exposure);

			// ErasRewardPoints (individual)
			reward_points_individual.insert(validator.clone(), 7u32);

			// ValidatorSlashInEra (first 33%)
			if i < slashed_validators {
				crate::ValidatorSlashInEra::<T>::insert(
					era,
					validator.clone(),
					(Perbill::from_percent(10), BalanceOf::<T>::max_value() / 10u32.into()),
				);
			}

			// ErasValidatorIncentiveWeight
			let incentive_weight = BalanceOf::<T>::from(100u64);
			ErasValidatorIncentiveWeight::<T>::insert(era, validator, incentive_weight);
			total_incentive_weight += incentive_weight;
		}

		// Single-entry storages
		ErasValidatorReward::<T>::insert(era, BalanceOf::<T>::max_value());
		ErasRewardPoints::<T>::insert(
			era,
			crate::EraRewardPoints::<T> {
				total: 77777,
				individual: reward_points_individual.try_into().unwrap(),
			},
		);
		ErasTotalStake::<T>::insert(era, BalanceOf::<T>::max_value());
		ErasNominatorsSlashable::<T>::insert(era, true);
		ErasSumValidatorIncentiveWeight::<T>::insert(era, total_incentive_weight);
		ErasSumWeightedPoints::<T>::insert(
			era,
			total_incentive_weight.saturating_mul(BalanceOf::<T>::from(7u32)),
		);
		ErasValidatorIncentiveBudget::<T>::insert(era, BalanceOf::<T>::from(1_000_000u64));

		era
	}

	/// Validates that the weight consumption of a pruning operation stays within expected limits.
	fn validate_pruning_weight<T: Config>(
		result: &frame_support::dispatch::DispatchResultWithPostInfo,
		step_name: &str,
		validator_count: u32,
	) {
		assert!(
			result.is_ok(),
			"Benchmark {} should succeed with v={}",
			step_name,
			validator_count
		);

		let post_info = result.unwrap();
		let actual_ref_time = post_info
			.actual_weight
			.expect(&format!(
				"Should report actual weight for {} with v={}",
				step_name, validator_count
			))
			.ref_time();

		assert!(
			actual_ref_time > 0,
			"Should report non-zero ref_time for {} with v={}",
			step_name,
			validator_count
		);
		// No need to validate against MaxPruningItems since we use item-based limiting
	}

	// Benchmark pruning ErasStakersPaged (first step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_stakers_paged(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(v);
		EraPruningState::<T>::insert(era, PruningStep::ErasStakersPaged);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ErasStakersPaged", v);

		Ok(())
	}

	// Benchmark pruning ErasStakersOverview (second step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_stakers_overview(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(v);
		EraPruningState::<T>::insert(era, PruningStep::ErasStakersOverview);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ErasStakersOverview", v);

		Ok(())
	}

	// Benchmark pruning ErasValidatorPrefs (third step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_validator_prefs(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(v);
		EraPruningState::<T>::insert(era, PruningStep::ErasValidatorPrefs);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ErasValidatorPrefs", v);

		Ok(())
	}

	// Benchmark pruning ClaimedRewards (fourth step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_claimed_rewards(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(v);
		EraPruningState::<T>::insert(era, PruningStep::ClaimedRewards);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ClaimedRewards", v);

		Ok(())
	}

	// Benchmark pruning ErasValidatorReward (fifth step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_validator_reward() -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(1);
		EraPruningState::<T>::insert(era, PruningStep::ErasValidatorReward);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ErasValidatorReward", 1);

		Ok(())
	}

	// Benchmark pruning ErasRewardPoints (sixth step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_reward_points() -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(1);
		EraPruningState::<T>::insert(era, PruningStep::ErasRewardPoints);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ErasRewardPoints", 1);

		Ok(())
	}

	// Benchmark pruning single-entry cleanups (seventh step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_single_entry_cleanups() -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(1);
		EraPruningState::<T>::insert(era, PruningStep::SingleEntryCleanups);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "SingleEntryCleanups", 1);

		Ok(())
	}

	// Benchmark pruning ValidatorSlashInEra (eighth step)
	#[benchmark(pov_mode = Measured)]
	fn prune_era_validator_slash_in_era(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(v);
		EraPruningState::<T>::insert(era, PruningStep::ValidatorSlashInEra);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ValidatorSlashInEra", v);

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn prune_era_validator_incentive_weight(
		v: Linear<1, { T::MaxValidatorSet::get() }>,
	) -> Result<(), BenchmarkError> {
		let era = setup_era_for_pruning::<T>(v);
		EraPruningState::<T>::insert(era, PruningStep::ErasValidatorIncentiveWeight);

		let caller: T::AccountId = whitelisted_caller();

		let result;
		#[block]
		{
			result = Pallet::<T>::prune_era_step(RawOrigin::Signed(caller).into(), era);
		}

		validate_pruning_weight::<T>(&result, "ErasValidatorIncentiveWeight", v);

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn chill_inactive(
		l: Linear<2, { ChillInactiveThreshold::<T>::get() }>,
	) -> Result<(), BenchmarkError> {
		let (stash, _, _) =
			create_validator_with_nominators::<T>(0, 0, false, true, RewardDestination::Staked)?;
		assert!(T::VoterList::contains(&stash));

		Staking::<T>::set_staking_configs(
			RawOrigin::Root.into(),
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(BalanceOf::<T>::max_value()),
			ConfigOp::Set(0),
			ConfigOp::Set(0),
			ConfigOp::Set(Percent::from_percent(0)),
			ConfigOp::Set(Zero::zero()),
			ConfigOp::Noop,
			ConfigOp::Noop,
			ConfigOp::Set(l),
		)?;

		let caller = whitelisted_caller();

		// Proof eras `0..l` must sit in `[active_era - HistoryDepth, active_era)`. `l <=
		// HistoryDepth`, so `active_era = l` keeps the lower bound at 0 while the most recent
		// proof era (`l - 1`) stays below the active era.
		set_active_era::<T>(l);

		// Fill the `EraRewardPoints::individual` to hit the worst-case scenario.
		let mut individual_points = BoundedBTreeMap::new();
		for index in 0..T::MaxValidatorSet::get() {
			individual_points
				.try_insert(account::<T::AccountId>("validator", index, 0), 100)
				.unwrap();
		}

		// Set the validator has been inactive for `l` eras.
		let proof = (0..l)
			.inspect(|&era| {
				ErasRewardPoints::<T>::insert(
					era,
					EraRewardPoints { total: 0, individual: individual_points.clone() },
				);
				Eras::<T>::upsert_exposure(era, &stash, Exposure::default());
			})
			.collect::<Vec<_>>();
		let proof = BoundedVec::truncate_from(proof);

		assert!(Validators::<T>::contains_key(&stash));

		#[extrinsic_call]
		_(RawOrigin::Signed(caller), stash.clone(), proof);

		assert!(!Validators::<T>::contains_key(&stash));

		Ok(())
	}

	impl_benchmark_test_suite!(
		Staking,
		crate::mock::ExtBuilder::default().has_stakers(true),
		crate::mock::Test,
		exec_name = build_and_execute
	);
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{ExtBuilder, RuntimeOrigin, Staking, Test};
	use frame_support::assert_ok;

	#[test]
	fn create_validators_with_nominators_for_era_works() {
		ExtBuilder::default().build_and_execute(|| {
			let v = 10;
			let n = 100;

			create_validators_with_nominators_for_era::<Test>(
				v,
				n,
				MaxNominationsOf::<Test>::get() as usize,
				false,
				None,
			)
			.unwrap();

			let count_validators = Validators::<Test>::iter().count();
			let count_nominators = Nominators::<Test>::iter().count();

			assert_eq!(count_validators, Validators::<Test>::count() as usize);
			assert_eq!(count_nominators, Nominators::<Test>::count() as usize);

			assert_eq!(count_validators, v as usize);
			assert_eq!(count_nominators, n as usize);
		});
	}

	#[test]
	fn create_validator_with_nominators_works() {
		ExtBuilder::default().build_and_execute(|| {
			let n = 10;

			let (validator_stash, nominators, current_era) =
				create_validator_with_nominators::<Test>(
					n,
					<<Test as Config>::MaxExposurePageSize as Get<_>>::get(),
					false,
					false,
					RewardDestination::Staked,
				)
				.unwrap();

			assert_eq!(nominators.len() as u32, n);

			let original_stakeable_balance = asset::stakeable_balance::<Test>(&validator_stash);
			assert_ok!(Staking::payout_stakers_by_page(
				RuntimeOrigin::signed(1337),
				validator_stash,
				current_era,
				0
			));
			let new_stakeable_balance = asset::stakeable_balance::<Test>(&validator_stash);

			// reward increases stakeable balance
			assert!(original_stakeable_balance < new_stakeable_balance);
		});
	}
}
