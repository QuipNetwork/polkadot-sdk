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

//! Benchmarking setup for pallet-collator-selection

#![cfg(feature = "runtime-benchmarks")]

use super::*;

#[allow(unused)]
use crate::Pallet as CollatorSelection;
use alloc::vec::Vec;
use core::cmp;
use cumulus_pallet_session_benchmarking as session_benchmarking;
use frame_benchmarking::{account, v2::*, whitelisted_caller, BenchmarkError};
use frame_support::traits::{Currency, EnsureOrigin, Get, ReservableCurrency};
use frame_system::{pallet_prelude::BlockNumberFor, EventRecord, RawOrigin};
use pallet_authorship::EventHandler;
use pallet_session::{self as session, SessionManager};

pub type BalanceOf<T> =
	<<T as super::Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

const SEED: u32 = 0;

fn assert_last_event<T: Config>(generic_event: <T as super::Config>::RuntimeEvent) {
	let events = frame_system::Pallet::<T>::events();
	let system_event: <T as frame_system::Config>::RuntimeEvent = generic_event.into();
	// compare to the last event record
	let EventRecord { event, .. } = &events[events.len() - 1];
	assert_eq!(event, &system_event);
}

fn create_funded_user<T: Config>(
	string: &'static str,
	n: u32,
	balance_factor: u32,
) -> T::AccountId {
	let user = account(string, n, SEED);
	let balance = <T as pallet::Config>::Currency::minimum_balance() * balance_factor.into();
	let _ = <T as pallet::Config>::Currency::make_free_balance_be(&user, balance);
	user
}

fn validator<T: Config + session_benchmarking::Config>(
	c: u32,
) -> (T::AccountId, <T as session::Config>::Keys, Vec<u8>) {
	let validator = create_funded_user::<T>("candidate", c, 1000);
	let (keys, proof) = T::generate_session_keys_and_proof(validator.clone());

	(validator, keys, proof)
}

fn register_validators<T: Config + session_benchmarking::Config>(count: u32) -> Vec<T::AccountId> {
	let validators = (0..count).map(|c| validator::<T>(c)).collect::<Vec<_>>();

	for (who, keys, proof) in validators.clone() {
		<session::Pallet<T>>::ensure_can_pay_key_deposit(&who).unwrap();
		<session::Pallet<T>>::set_keys(RawOrigin::Signed(who).into(), keys, proof).unwrap();
	}

	validators.into_iter().map(|(who, _, _)| who).collect()
}

fn register_candidates<T: Config>(count: u32) {
	let candidates = (0..count).map(|c| account("candidate", c, SEED)).collect::<Vec<_>>();
	assert!(CandidacyBond::<T>::get() > 0u32.into(), "Bond cannot be zero!");

	for who in candidates {
		<T as pallet::Config>::Currency::make_free_balance_be(
			&who,
			CandidacyBond::<T>::get() * 3u32.into(),
		);
		<CollatorSelection<T>>::register_as_candidate(RawOrigin::Signed(who).into()).unwrap();
	}
}

fn min_candidates<T: Config>() -> u32 {
	let min_collators = T::MinEligibleCollators::get();
	let invulnerable_length = Invulnerables::<T>::get().len();
	min_collators.saturating_sub(invulnerable_length.try_into().unwrap())
}

fn min_invulnerables<T: Config>() -> u32 {
	let min_collators = T::MinEligibleCollators::get();
	let candidates_length = CandidateList::<T>::decode_len()
		.unwrap_or_default()
		.try_into()
		.unwrap_or_default();
	min_collators.saturating_sub(candidates_length)
}

#[benchmarks(where T: pallet_authorship::Config + session_benchmarking::Config)]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn set_invulnerables(
		b: Linear<1, { T::MaxInvulnerables::get() }>,
	) -> Result<(), BenchmarkError> {
		let origin =
			T::UpdateOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		let new_invulnerables = register_validators::<T>(b);
		let mut sorted_new_invulnerables = new_invulnerables.clone();
		sorted_new_invulnerables.sort();

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, new_invulnerables.clone());

		// assert that it comes out sorted
		assert_last_event::<T>(
			Event::NewInvulnerables { invulnerables: sorted_new_invulnerables }.into(),
		);
		Ok(())
	}

	#[benchmark]
	fn add_invulnerable(
		b: Linear<1, { T::MaxInvulnerables::get() - 1 }>,
		c: Linear<1, { T::MaxCandidates::get() - 1 }>,
	) -> Result<(), BenchmarkError> {
		let origin =
			T::UpdateOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		// need to fill up candidates
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		DesiredCandidates::<T>::put(c);
		// get accounts and keys for the `c` candidates
		let mut candidates = (0..c).map(|cc| validator::<T>(cc)).collect::<Vec<_>>();
		// add one more to the list. should not be in `b` (invulnerables) because it's the account
		// we will _add_ to invulnerables. we want it to be in `candidates` because we need the
		// weight associated with removing it.
		let (new_invulnerable, new_invulnerable_keys, new_proof) = validator::<T>(b.max(c) + 1);
		candidates.push((new_invulnerable.clone(), new_invulnerable_keys, new_proof));
		// set their keys ...
		for (who, keys, proof) in candidates.clone() {
			<session::Pallet<T>>::ensure_can_pay_key_deposit(&who).unwrap();
			<session::Pallet<T>>::set_keys(RawOrigin::Signed(who).into(), keys, proof).unwrap();
		}
		// ... and register them.
		for (who, _, _) in candidates.iter() {
			let deposit = CandidacyBond::<T>::get();
			<T as pallet::Config>::Currency::make_free_balance_be(who, deposit * 1000_u32.into());
			CandidateList::<T>::try_mutate(|list| {
				list.try_push(CandidateInfo { who: who.clone(), deposit }).unwrap();
				Ok::<(), BenchmarkError>(())
			})
			.unwrap();
			<T as pallet::Config>::Currency::reserve(who, deposit)?;
			LastAuthoredBlock::<T>::insert(
				who.clone(),
				frame_system::Pallet::<T>::block_number() + T::KickThreshold::get(),
			);
		}

		// now we need to fill up invulnerables
		let mut invulnerables = register_validators::<T>(b);
		invulnerables.sort();
		let invulnerables: frame_support::BoundedVec<_, T::MaxInvulnerables> =
			frame_support::BoundedVec::try_from(invulnerables).unwrap();
		Invulnerables::<T>::put(invulnerables);

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, new_invulnerable.clone());

		assert_last_event::<T>(Event::InvulnerableAdded { account_id: new_invulnerable }.into());
		Ok(())
	}

	#[benchmark]
	fn remove_invulnerable(
		b: Linear<{ min_invulnerables::<T>() + 1 }, { T::MaxInvulnerables::get() }>,
	) -> Result<(), BenchmarkError> {
		let origin =
			T::UpdateOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		let mut invulnerables = register_validators::<T>(b);
		invulnerables.sort();
		let invulnerables: frame_support::BoundedVec<_, T::MaxInvulnerables> =
			frame_support::BoundedVec::try_from(invulnerables).unwrap();
		Invulnerables::<T>::put(invulnerables);
		let to_remove = Invulnerables::<T>::get().first().unwrap().clone();

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, to_remove.clone());

		assert_last_event::<T>(Event::InvulnerableRemoved { account_id: to_remove }.into());
		Ok(())
	}

	#[benchmark]
	fn set_desired_candidates() -> Result<(), BenchmarkError> {
		let max: u32 = T::MaxCandidates::get();
		let origin =
			T::UpdateOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, max);

		assert_last_event::<T>(Event::NewDesiredCandidates { desired_candidates: max }.into());
		Ok(())
	}

	#[benchmark]
	fn set_candidacy_bond(
		c: Linear<0, { T::MaxCandidates::get() }>,
		k: Linear<0, { T::MaxCandidates::get() }>,
	) -> Result<(), BenchmarkError> {
		let initial_bond_amount: BalanceOf<T> =
			<T as pallet::Config>::Currency::minimum_balance() * 2u32.into();
		CandidacyBond::<T>::put(initial_bond_amount);
		register_validators::<T>(c);
		register_candidates::<T>(c);
		let kicked = cmp::min(k, c);
		let origin =
			T::UpdateOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		let bond_amount = if k > 0 {
			CandidateList::<T>::mutate(|candidates| {
				for info in candidates.iter_mut().skip(kicked as usize) {
					info.deposit = <T as pallet::Config>::Currency::minimum_balance() * 3u32.into();
				}
			});
			<T as pallet::Config>::Currency::minimum_balance() * 3u32.into()
		} else {
			<T as pallet::Config>::Currency::minimum_balance()
		};

		#[extrinsic_call]
		_(origin as T::RuntimeOrigin, bond_amount);

		assert_last_event::<T>(Event::NewCandidacyBond { bond_amount }.into());
		Ok(())
	}

	#[benchmark]
	fn update_bond(
		c: Linear<{ min_candidates::<T>() + 1 }, { T::MaxCandidates::get() }>,
	) -> Result<(), BenchmarkError> {
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		DesiredCandidates::<T>::put(c);

		register_validators::<T>(c);
		register_candidates::<T>(c);

		let caller = CandidateList::<T>::get()[0].who.clone();
		v2::whitelist!(caller);

		let bond_amount: BalanceOf<T> = <T as pallet::Config>::Currency::minimum_balance() +
			<T as pallet::Config>::Currency::minimum_balance();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), bond_amount);

		assert_last_event::<T>(
			Event::CandidateBondUpdated { account_id: caller, deposit: bond_amount }.into(),
		);
		assert!(
			CandidateList::<T>::get().iter().last().unwrap().deposit ==
				<T as pallet::Config>::Currency::minimum_balance() * 2u32.into()
		);
		Ok(())
	}

	// worse case is when we have all the max-candidate slots filled except one, and we fill that
	// one.
	#[benchmark]
	fn register_as_candidate(c: Linear<1, { T::MaxCandidates::get() - 1 }>) {
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		DesiredCandidates::<T>::put(c + 1);

		register_validators::<T>(c);
		register_candidates::<T>(c);

		let caller: T::AccountId = whitelisted_caller();
		let bond: BalanceOf<T> = <T as pallet::Config>::Currency::minimum_balance() * 2u32.into();
		<T as pallet::Config>::Currency::make_free_balance_be(&caller, bond);
		let (keys, proof) = T::generate_session_keys_and_proof(caller.clone());

		<session::Pallet<T>>::ensure_can_pay_key_deposit(&caller).unwrap();
		<session::Pallet<T>>::set_keys(RawOrigin::Signed(caller.clone()).into(), keys, proof)
			.unwrap();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()));

		assert_last_event::<T>(
			Event::CandidateAdded { account_id: caller, deposit: bond / 2u32.into() }.into(),
		);
	}

	#[benchmark]
	fn take_candidate_slot(c: Linear<{ min_candidates::<T>() + 1 }, { T::MaxCandidates::get() }>) {
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		DesiredCandidates::<T>::put(1);

		register_validators::<T>(c);
		register_candidates::<T>(c);

		let caller: T::AccountId = whitelisted_caller();
		let bond: BalanceOf<T> = <T as pallet::Config>::Currency::minimum_balance() * 10u32.into();
		<T as pallet::Config>::Currency::make_free_balance_be(&caller, bond);

		let (keys, proof) = T::generate_session_keys_and_proof(caller.clone());

		<session::Pallet<T>>::ensure_can_pay_key_deposit(&caller).unwrap();
		<session::Pallet<T>>::set_keys(RawOrigin::Signed(caller.clone()).into(), keys, proof)
			.unwrap();

		let target = CandidateList::<T>::get().iter().last().unwrap().who.clone();

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), bond / 2u32.into(), target.clone());

		assert_last_event::<T>(
			Event::CandidateReplaced { old: target, new: caller, deposit: bond / 2u32.into() }
				.into(),
		);
	}

	// worse case is the last candidate leaving.
	#[benchmark]
	fn leave_intent(c: Linear<{ min_candidates::<T>() + 1 }, { T::MaxCandidates::get() }>) {
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		DesiredCandidates::<T>::put(c);

		register_validators::<T>(c);
		register_candidates::<T>(c);

		let leaving = CandidateList::<T>::get().iter().last().unwrap().who.clone();
		v2::whitelist!(leaving);

		#[extrinsic_call]
		_(RawOrigin::Signed(leaving.clone()));

		assert_last_event::<T>(Event::CandidateRemoved { account_id: leaving }.into());
	}

	// worse case is paying a non-existing candidate account.
	#[benchmark]
	fn note_author() {
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		<T as pallet::Config>::Currency::make_free_balance_be(
			&<CollatorSelection<T>>::account_id(),
			<T as pallet::Config>::Currency::minimum_balance() * 4u32.into(),
		);
		let author = account("author", 0, SEED);
		let new_block: BlockNumberFor<T> = 10u32.into();

		frame_system::Pallet::<T>::set_block_number(new_block);
		assert!(<T as pallet::Config>::Currency::free_balance(&author) == 0u32.into());

		#[block]
		{
			<CollatorSelection<T> as EventHandler<_, _>>::note_author(author.clone())
		}

		assert!(<T as pallet::Config>::Currency::free_balance(&author) > 0u32.into());
		assert_eq!(frame_system::Pallet::<T>::block_number(), new_block);
	}

	// worst case for new session.
	#[benchmark]
	fn new_session(
		r: Linear<1, { T::MaxCandidates::get() }>,
		c: Linear<1, { T::MaxCandidates::get() }>,
	) {
		CandidacyBond::<T>::put(<T as pallet::Config>::Currency::minimum_balance());
		DesiredCandidates::<T>::put(c);
		frame_system::Pallet::<T>::set_block_number(0u32.into());

		register_validators::<T>(c);
		register_candidates::<T>(c);

		let new_block: BlockNumberFor<T> = T::KickThreshold::get();
		let zero_block: BlockNumberFor<T> = 0u32.into();
		let candidates: Vec<T::AccountId> = CandidateList::<T>::get()
			.iter()
			.map(|candidate_info| candidate_info.who.clone())
			.collect();

		let non_removals = c.saturating_sub(r);

		for i in 0..c {
			LastAuthoredBlock::<T>::insert(candidates[i as usize].clone(), zero_block);
		}

		if non_removals > 0 {
			for i in 0..non_removals {
				LastAuthoredBlock::<T>::insert(candidates[i as usize].clone(), new_block);
			}
		} else {
			for i in 0..c {
				LastAuthoredBlock::<T>::insert(candidates[i as usize].clone(), new_block);
			}
		}

		let min_candidates = min_candidates::<T>();
		let pre_length = CandidateList::<T>::decode_len().unwrap_or_default();

		frame_system::Pallet::<T>::set_block_number(new_block);

		let current_length: u32 = CandidateList::<T>::decode_len()
			.unwrap_or_default()
			.try_into()
			.unwrap_or_default();
		assert!(c == current_length);
		#[block]
		{
			<CollatorSelection<T> as SessionManager<_>>::new_session(0);
		}

		if c > r && non_removals >= min_candidates {
			// candidates > removals and remaining candidates > min candidates
			// => remaining candidates should be shorter than before removal, i.e. some were
			//    actually removed.
			assert!(CandidateList::<T>::decode_len().unwrap_or_default() < pre_length);
		} else if c > r && non_removals < min_candidates {
			// candidates > removals and remaining candidates would be less than min candidates
			// => remaining candidates should equal min candidates, i.e. some were removed up to
			//    the minimum, but then any more were "forced" to stay in candidates.
			let current_length: u32 = CandidateList::<T>::decode_len()
				.unwrap_or_default()
				.try_into()
				.unwrap_or_default();
			assert!(min_candidates == current_length);
		} else {
			// removals >= candidates, non removals must == 0
			// can't remove more than exist
			assert!(CandidateList::<T>::decode_len().unwrap_or_default() == pre_length);
		}
	}

	impl_benchmark_test_suite!(
		CollatorSelection,
		{
			// Larger than the candidacy bonds used above.
			crate::mock::KeyDeposit::set(100);
			crate::mock::new_test_ext()
		},
		crate::mock::Test
	);
}
