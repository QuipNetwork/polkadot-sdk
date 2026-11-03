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
	signed::RewardSource,
	verifier::{Config, Event, FeasibilityError, Pallet, Status, StatusStorage},
	CurrentPhase, Phase,
};
use frame_benchmarking::v2::*;
use frame_election_provider_support::{ElectionProvider, NposSolution};
use frame_support::{
	pallet_prelude::*,
	traits::fungible::{Inspect, Mutate},
};
use sp_runtime::traits::Saturating;
use sp_std::prelude::*;

/// The currency configured on the signed pallet, which is what reward payouts are drawn from.
type SignedCurrencyOf<T> = <T as crate::signed::Config>::Currency;

#[benchmarks(where
	T: crate::Config + crate::signed::Config + crate::unsigned::Config,
	<T as frame_system::Config>::RuntimeEvent: TryInto<crate::verifier::Event<T>>
)]
mod benchmarks {
	use super::*;

	fn events_for<T: Config>() -> Vec<Event<T>>
	where
		<T as frame_system::Config>::RuntimeEvent: TryInto<Event<T>>,
	{
		frame_system::Pallet::<T>::read_events_for_pallet::<Event<T>>()
	}

	#[benchmark(pov_mode = Measured)]
	fn verification_valid_non_terminal() -> Result<(), BenchmarkError> {
		#[cfg(test)]
		crate::mock::ElectionStart::set(sp_runtime::traits::Bounded::max_value());
		crate::Pallet::<T>::start().unwrap();

		// roll to signed validation, with a solution stored in the signed pallet
		crate::Pallet::<T>::roll_to_signed_and_submit_full_solution()?;

		// roll to verification
		crate::Pallet::<T>::roll_until_matches(|| {
			matches!(CurrentPhase::<T>::get(), Phase::SignedValidation(_))
		});

		// start signal must have been sent by now
		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::msp()));

		#[block]
		{
			crate::Pallet::<T>::roll_next(false);
		}
		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::msp() - 1));

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn verification_valid_terminal() -> Result<(), BenchmarkError> {
		#[cfg(test)]
		crate::mock::ElectionStart::set(sp_runtime::traits::Bounded::max_value());
		crate::Pallet::<T>::start().unwrap();

		// roll to signed validation, with a solution stored in the signed pallet
		assert!(
			T::SignedValidationPhase::get() >= T::Pages::get().into(),
			"Signed validation phase must be larger than the number of pages"
		);

		crate::Pallet::<T>::roll_to_signed_and_submit_full_solution()?;

		// Accepting the solution pays the winner out of `RewardSource`, which holds nothing in
		// genesis. Fund it so the measured path is the transfer that production takes every era,
		// rather than the failed-transfer-then-defer fallback. The payout is `RewardBase` plus the
		// submission's accrued fee, so fund generously; the measured work does not depend on how
		// much the source holds.
		if let Some(source) = <T as crate::signed::Config>::RewardSource::account() {
			let funds = <T as crate::signed::Config>::RewardBase::get()
				.saturating_mul(2u32.into())
				.saturating_add(SignedCurrencyOf::<T>::minimum_balance());
			SignedCurrencyOf::<T>::mint_into(&source, funds)?;
		}

		// roll to before the last page of verification
		crate::Pallet::<T>::roll_until_matches(|| {
			matches!(CurrentPhase::<T>::get(), Phase::SignedValidation(_))
		});

		// start signal must have been sent by now
		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::msp()));
		for _ in 0..(T::Pages::get() - 1) {
			crate::Pallet::<T>::roll_next(false);
		}

		// we must have verified all pages by now, minus the last one.
		assert!(matches!(
			&events_for::<T>()[..],
			[Event::Verified(_, _), .., Event::Verified(1, _)]
		));

		// verify the last page.
		#[block]
		{
			crate::Pallet::<T>::roll_next(false);
		}

		// we are done
		assert_eq!(StatusStorage::<T>::get(), Status::Nothing);
		// last event is success
		assert!(matches!(
			&events_for::<T>()[..],
			[Event::Verified(_, _), .., Event::Verified(0, _), Event::Queued(_, None)]
		));

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn verification_invalid_terminal() -> Result<(), BenchmarkError> {
		// this is the verification of the current page + removing all of the previously valid
		// pages. The worst case is therefore when the last page is invalid, for example the final
		// score.
		assert!(T::Pages::get() >= 2, "benchmark only works if we have more than 2 pages");

		#[cfg(test)]
		crate::mock::ElectionStart::set(sp_runtime::traits::Bounded::max_value());
		crate::Pallet::<T>::start().unwrap();

		// roll to signed validation, with a solution stored in the signed pallet

		// but this solution is corrupt
		let mut paged_solution = crate::Pallet::<T>::roll_to_signed_and_mine_full_solution();
		paged_solution.score.minimal_stake -= 1;
		crate::Pallet::<T>::submit_full_solution(paged_solution)?;

		// roll to verification
		crate::Pallet::<T>::roll_until_matches(|| {
			matches!(CurrentPhase::<T>::get(), Phase::SignedValidation(_))
		});

		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::msp()));
		// verify all pages, except for the last one.
		for i in 0..T::Pages::get() - 1 {
			crate::Pallet::<T>::roll_next(false);
			assert_eq!(
				StatusStorage::<T>::get(),
				Status::Ongoing(crate::Pallet::<T>::msp() - 1 - i)
			);
		}

		// next page to be verified is the last one
		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::lsp()));
		assert!(matches!(
			&events_for::<T>()[..],
			[Event::Verified(_, _), .., Event::Verified(1, _)]
		));

		#[block]
		{
			crate::Pallet::<T>::roll_next(false);
		}

		// we are now reset.
		assert_eq!(StatusStorage::<T>::get(), Status::Nothing);
		assert!(matches!(
			&events_for::<T>()[..],
			[
				..,
				Event::Verified(0, _),
				Event::VerificationFailed(0, FeasibilityError::InvalidScore)
			]
		));

		Ok(())
	}

	#[benchmark(pov_mode = Measured)]
	fn verification_invalid_non_terminal(
		// number of valid pages that have been verified, before we verify the non-terminal invalid
		// page.
		v: Linear<0, { T::Pages::get() - 1 }>,
	) -> Result<(), BenchmarkError> {
		assert!(T::Pages::get() >= 2, "benchmark only works if we have more than 2 pages");

		#[cfg(test)]
		crate::mock::ElectionStart::set(sp_runtime::traits::Bounded::max_value());
		crate::Pallet::<T>::start().unwrap();

		// roll to signed validation, with a solution stored in the signed pallet, but this solution
		// is corrupt in its msp.
		let mut paged_solution = crate::Pallet::<T>::roll_to_signed_and_mine_full_solution();
		let page_to_corrupt = crate::Pallet::<T>::msp() - v;
		crate::log!(
			info,
			"pages of solution: {:?}, to corrupt {}, v {}",
			paged_solution.solution_pages.len(),
			page_to_corrupt,
			v
		);
		paged_solution.solution_pages[page_to_corrupt as usize].corrupt();
		crate::Pallet::<T>::submit_full_solution(paged_solution)?;

		// roll to verification
		crate::Pallet::<T>::roll_until_matches(|| {
			matches!(CurrentPhase::<T>::get(), Phase::SignedValidation(_))
		});

		// we should be ready to go
		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::msp()));

		// validate the the parameterized number of valid pages.
		for _ in 0..v {
			crate::Pallet::<T>::roll_next(false);
		}

		// we are still ready to continue
		assert_eq!(StatusStorage::<T>::get(), Status::Ongoing(crate::Pallet::<T>::msp() - v));

		// verify one page, which will be invalid.
		#[block]
		{
			crate::Pallet::<T>::roll_next(false);
		}

		// we are now reset, because this page was invalid.
		assert_eq!(StatusStorage::<T>::get(), Status::Nothing);

		assert!(matches!(
			&events_for::<T>()[..],
			[.., Event::VerificationFailed(_, FeasibilityError::NposElection(_))]
		));

		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::ExtBuilder::full().build_unchecked(),
		crate::mock::Runtime
	);
}
