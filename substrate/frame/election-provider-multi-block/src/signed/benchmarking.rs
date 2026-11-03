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
	signed::{Config, Invulnerables, Pallet, RewardSource, Submissions, MAX_UNPAID_REWARDS},
	types::PagedRawSolution,
	unsigned::miner::OffchainWorkerMiner,
	CurrentPhase, Phase, Round,
};
use frame_benchmarking::v2::*;
use frame_election_provider_support::ElectionProvider;
use frame_support::{
	pallet_prelude::*,
	traits::fungible::{Inspect, Mutate},
};
use frame_system::RawOrigin;
use sp_npos_elections::ElectionScore;
use sp_runtime::traits::{One, Saturating};
use sp_std::boxed::Box;

#[benchmarks(where T: crate::Config + crate::verifier::Config + crate::unsigned::Config)]
mod benchmarks {
	use super::*;

	#[benchmark(pov_mode = Measured)]
	fn register_not_full() -> Result<(), BenchmarkError> {
		CurrentPhase::<T>::put(Phase::Signed(T::SignedPhase::get() - One::one()));
		let round = Round::<T>::get();
		let alice = crate::Pallet::<T>::funded_account("alice", 0);
		let score = ElectionScore::default();

		assert_eq!(Submissions::<T>::sorted_submitters(round).len(), 0);
		#[block]
		{
			Pallet::<T>::register(RawOrigin::Signed(alice).into(), score)?;
		}

		assert_eq!(Submissions::<T>::sorted_submitters(round).len(), 1);
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn register_eject() -> Result<(), BenchmarkError> {
		CurrentPhase::<T>::put(Phase::Signed(T::SignedPhase::get() - One::one()));
		let round = Round::<T>::get();

		for i in 0..T::MaxSubmissions::get() {
			let submitter = crate::Pallet::<T>::funded_account("submitter", i);
			let score = ElectionScore { minimal_stake: i.into(), ..Default::default() };
			Pallet::<T>::register(RawOrigin::Signed(submitter.clone()).into(), score)?;

			// The first one, which will be ejected, has also submitted all pages
			if i == 0 {
				for p in 0..T::Pages::get() {
					let page = Some(Default::default());
					Pallet::<T>::submit_page(RawOrigin::Signed(submitter.clone()).into(), p, page)?;
				}
			}
		}

		let who = crate::Pallet::<T>::funded_account("who", 0);
		let score =
			ElectionScore { minimal_stake: T::MaxSubmissions::get().into(), ..Default::default() };

		assert_eq!(
			Submissions::<T>::sorted_submitters(round).len(),
			T::MaxSubmissions::get() as usize
		);

		#[block]
		{
			Pallet::<T>::register(RawOrigin::Signed(who).into(), score)?;
		}

		assert_eq!(
			Submissions::<T>::sorted_submitters(round).len(),
			T::MaxSubmissions::get() as usize
		);
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn submit_page() -> Result<(), BenchmarkError> {
		#[cfg(test)]
		crate::mock::ElectionStart::set(sp_runtime::traits::Bounded::max_value());
		crate::Pallet::<T>::start().unwrap();

		crate::Pallet::<T>::roll_until_matches(|| {
			matches!(CurrentPhase::<T>::get(), Phase::Signed(_))
		});

		// mine a full solution
		let PagedRawSolution { score, solution_pages, .. } =
			OffchainWorkerMiner::<T>::mine_solution(T::Pages::get(), false).unwrap();
		let page = Some(Box::new(solution_pages[0].clone()));

		// register alice
		let alice = crate::Pallet::<T>::funded_account("alice", 0);
		Pallet::<T>::register(RawOrigin::Signed(alice.clone()).into(), score)?;

		#[block]
		{
			Pallet::<T>::submit_page(RawOrigin::Signed(alice).into(), 0, page)?;
		}

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn unset_page() -> Result<(), BenchmarkError> {
		#[cfg(test)]
		crate::mock::ElectionStart::set(sp_runtime::traits::Bounded::max_value());
		crate::Pallet::<T>::start().unwrap();

		crate::Pallet::<T>::roll_until_matches(|| {
			matches!(CurrentPhase::<T>::get(), Phase::Signed(_))
		});

		// mine a full solution
		let PagedRawSolution { score, solution_pages, .. } =
			OffchainWorkerMiner::<T>::mine_solution(T::Pages::get(), false).unwrap();
		let page = Some(Box::new(solution_pages[0].clone()));

		// register alice
		let alice = crate::Pallet::<T>::funded_account("alice", 0);
		Pallet::<T>::register(RawOrigin::Signed(alice.clone()).into(), score)?;

		// submit page
		Pallet::<T>::submit_page(RawOrigin::Signed(alice.clone()).into(), 0, page)?;

		#[block]
		{
			Pallet::<T>::submit_page(RawOrigin::Signed(alice).into(), 0, None)?;
		}

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn bail() -> Result<(), BenchmarkError> {
		CurrentPhase::<T>::put(Phase::Signed(T::SignedPhase::get() - One::one()));
		let alice = crate::Pallet::<T>::funded_account("alice", 0);

		// register alice
		let score = ElectionScore::default();
		Pallet::<T>::register(RawOrigin::Signed(alice.clone()).into(), score)?;

		// submit all pages
		for p in 0..T::Pages::get() {
			let page = Some(Default::default());
			Pallet::<T>::submit_page(RawOrigin::Signed(alice.clone()).into(), p, page)?;
		}

		#[block]
		{
			Pallet::<T>::bail(RawOrigin::Signed(alice).into())?;
		}

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn clear_old_round_data(p: Linear<1, { T::Pages::get() }>) -> Result<(), BenchmarkError> {
		// set signed phase and alice ready to submit
		CurrentPhase::<T>::put(Phase::Signed(T::SignedPhase::get() - One::one()));
		let alice = crate::Pallet::<T>::funded_account("alice", 0);

		// worst case: an invulnerable is also refunded their tx-fee upon clearing.
		Invulnerables::<T>::put(BoundedVec::truncate_from(sp_std::vec![alice.clone()]));

		// register alice
		let score = ElectionScore::default();
		Pallet::<T>::register(RawOrigin::Signed(alice.clone()).into(), score)?;

		// submit a solution with p pages.
		for pp in 0..p {
			let page = Some(Default::default());
			Pallet::<T>::submit_page(RawOrigin::Signed(alice.clone()).into(), pp, page)?;
		}

		// force rotate to the next round.
		let prev_round = Round::<T>::get();
		crate::Pallet::<T>::rotate_round();

		let source_and_balance_before = if let Some(source) = T::RewardSource::account() {
			let funds =
				<T as Config>::MaxFeeRefund::get().saturating_add(T::Currency::minimum_balance());
			T::Currency::mint_into(&source, funds)?;
			Some((source.clone(), T::Currency::balance(&source)))
		} else {
			None
		};

		#[block]
		{
			Pallet::<T>::clear_old_round_data(RawOrigin::Signed(alice).into(), prev_round, p)?;
		}

		if let Some((source, balance_before)) = source_and_balance_before {
			assert!(
				T::Currency::balance(&source) < balance_before,
				"fee refund must have transferred out of the configured RewardSource pot"
			);
		}
		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn claim_unpaid_reward() -> Result<(), BenchmarkError> {
		// Worst case: UnpaidRewards is full, and the claimed entry is the last one scanned.
		for i in 0..MAX_UNPAID_REWARDS {
			let who = crate::Pallet::<T>::funded_account("filler", i);
			let entry = crate::signed::UnpaidReward::<T> {
				round: i,
				who,
				amount: <T as Config>::RewardBase::get(),
			};
			crate::signed::UnpaidRewards::<T>::try_mutate(|unpaid| unpaid.try_push(entry))
				.map_err(|_| BenchmarkError::Stop("UnpaidRewards is full"))?;
		}
		let target_round = MAX_UNPAID_REWARDS - 1;

		// The claim pays out of `RewardSource`, so it must be able to cover one entry and still
		// hold ED afterwards, as the payout uses `Preservation::Preserve`. A `None` source mints
		// and needs no funding.
		let source_and_balance_before = if let Some(source) = T::RewardSource::account() {
			let funds =
				<T as Config>::RewardBase::get().saturating_add(T::Currency::minimum_balance());
			T::Currency::mint_into(&source, funds)?;
			Some((source.clone(), T::Currency::balance(&source)))
		} else {
			None
		};

		let caller = crate::Pallet::<T>::funded_account("caller", 0);

		#[block]
		{
			Pallet::<T>::claim_unpaid_reward(RawOrigin::Signed(caller).into(), target_round)?;
		}

		assert_eq!(crate::signed::UnpaidRewards::<T>::get().len(), MAX_UNPAID_REWARDS as usize - 1);
		// Guard against silently measuring the mint fallback instead of the real transfer: if a
		// pot is configured, its balance must have dropped by the claimed amount.
		if let Some((source, balance_before)) = source_and_balance_before {
			assert!(
				T::Currency::balance(&source) < balance_before,
				"claim must have transferred out of the configured RewardSource pot"
			);
		}
		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::ExtBuilder::signed().build_unchecked(),
		crate::mock::Runtime
	);
}
