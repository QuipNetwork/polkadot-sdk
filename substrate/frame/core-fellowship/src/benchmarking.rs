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

//! Core Fellowship pallet benchmarking.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use crate::{BlockNumberFor as CoreFellowshipBlockNumberFor, Pallet as CoreFellowship};

use alloc::{boxed::Box, vec};
use frame_benchmarking::v2::*;
use frame_system::RawOrigin;
use sp_arithmetic::traits::Bounded;
use sp_runtime::traits::BlockNumberProvider;

const SEED: u32 = 0;

type BenchResult = Result<(), BenchmarkError>;

#[instance_benchmarks]
mod benchmarks {
	use super::*;

	fn set_block_number<T: Config<I>, I: 'static>(n: CoreFellowshipBlockNumberFor<T, I>) {
		T::BlockNumberProvider::set_block_number(n);
	}

	fn ensure_evidence<T: Config<I>, I: 'static>(who: &T::AccountId) -> BenchResult {
		let evidence = BoundedVec::try_from(vec![0; Evidence::<T, I>::bound()]).unwrap();
		let wish = Wish::Retention;
		let origin = RawOrigin::Signed(who.clone()).into();
		CoreFellowship::<T, I>::submit_evidence(origin, wish, evidence)?;
		assert!(MemberEvidence::<T, I>::contains_key(who));
		Ok(())
	}

	fn make_member<T: Config<I>, I: 'static>(rank: u16) -> Result<T::AccountId, BenchmarkError> {
		let member = account("member", 0, SEED);
		T::Members::induct(&member)?;
		for _ in 0..rank {
			T::Members::promote(&member)?;
		}
		#[allow(deprecated)]
		CoreFellowship::<T, I>::import(RawOrigin::Signed(member.clone()).into())?;
		Ok(member)
	}

	fn set_benchmark_params<T: Config<I>, I: 'static>() -> Result<(), BenchmarkError> {
		let max_rank = T::MaxRank::get() as usize;
		let params = ParamsType {
			active_salary: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			passive_salary: BoundedVec::try_from(vec![10u32.into(); max_rank]).unwrap(),
			demotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			min_promotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			offboard_timeout: 1u32.into(),
		};

		CoreFellowship::<T, I>::set_params(RawOrigin::Root.into(), Box::new(params))?;
		Ok(())
	}

	#[benchmark]
	fn set_params() -> Result<(), BenchmarkError> {
		let max_rank = T::MaxRank::get() as usize;
		let params = ParamsType {
			active_salary: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			passive_salary: BoundedVec::try_from(vec![10u32.into(); max_rank]).unwrap(),
			demotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			min_promotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			offboard_timeout: 1u32.into(),
		};

		#[extrinsic_call]
		_(RawOrigin::Root, Box::new(params.clone()));

		assert_eq!(Params::<T, I>::get(), params);
		Ok(())
	}

	#[benchmark]
	fn set_partial_params() -> Result<(), BenchmarkError> {
		let max_rank = T::MaxRank::get() as usize;

		// Set up the initial default state for the Params storage
		let params = ParamsType {
			active_salary: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			passive_salary: BoundedVec::try_from(vec![10u32.into(); max_rank]).unwrap(),
			demotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			min_promotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			offboard_timeout: 1u32.into(),
		};
		CoreFellowship::<T, I>::set_params(RawOrigin::Root.into(), Box::new(params))?;

		let default_params = Params::<T, I>::get();
		let expected_params = ParamsType {
			active_salary: default_params.active_salary,
			passive_salary: BoundedVec::try_from(vec![10u32.into(); max_rank]).unwrap(),
			demotion_period: default_params.demotion_period,
			min_promotion_period: BoundedVec::try_from(vec![100u32.into(); max_rank]).unwrap(),
			offboard_timeout: 1u32.into(),
		};

		let params_payload = ParamsType {
			active_salary: BoundedVec::try_from(vec![None; max_rank]).unwrap(),
			passive_salary: BoundedVec::try_from(vec![Some(10u32.into()); max_rank]).unwrap(),
			demotion_period: BoundedVec::try_from(vec![None; max_rank]).unwrap(),
			min_promotion_period: BoundedVec::try_from(vec![Some(100u32.into()); max_rank])
				.unwrap(),
			offboard_timeout: None,
		};

		#[extrinsic_call]
		_(RawOrigin::Root, Box::new(params_payload.clone()));

		assert_eq!(Params::<T, I>::get(), expected_params);
		Ok(())
	}

	#[benchmark]
	fn bump_offboard() -> Result<(), BenchmarkError> {
		set_benchmark_params::<T, I>()?;

		let member = make_member::<T, I>(0)?;

		// Set it to the max value to ensure that any possible auto-demotion period has passed.
		set_block_number::<T, I>(CoreFellowshipBlockNumberFor::<T, I>::max_value());
		ensure_evidence::<T, I>(&member)?;
		assert!(Member::<T, I>::contains_key(&member));

		#[extrinsic_call]
		CoreFellowship::<T, I>::bump(RawOrigin::Signed(member.clone()), member.clone());

		assert!(!Member::<T, I>::contains_key(&member));
		assert!(!MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn bump_demote() -> Result<(), BenchmarkError> {
		set_benchmark_params::<T, I>()?;

		let initial_rank = T::MaxRank::get();

		let member = make_member::<T, I>(initial_rank)?;

		// Set it to the max value to ensure that any possible auto-demotion period has passed.
		set_block_number::<T, I>(CoreFellowshipBlockNumberFor::<T, I>::max_value());
		ensure_evidence::<T, I>(&member)?;

		assert!(Member::<T, I>::contains_key(&member));
		assert_eq!(T::Members::rank_of(&member), Some(initial_rank));

		#[extrinsic_call]
		CoreFellowship::<T, I>::bump(RawOrigin::Signed(member.clone()), member.clone());

		assert!(Member::<T, I>::contains_key(&member));
		assert_eq!(T::Members::rank_of(&member), Some(initial_rank.saturating_sub(1)));
		assert!(!MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn set_active() -> Result<(), BenchmarkError> {
		let member = make_member::<T, I>(1)?;
		assert!(Member::<T, I>::get(&member).unwrap().is_active);

		#[extrinsic_call]
		_(RawOrigin::Signed(member.clone()), false);

		assert!(!Member::<T, I>::get(&member).unwrap().is_active);
		Ok(())
	}

	#[benchmark]
	fn induct() -> Result<(), BenchmarkError> {
		let candidate: T::AccountId = account("candidate", 0, SEED);

		#[extrinsic_call]
		_(RawOrigin::Root, candidate.clone());

		assert_eq!(T::Members::rank_of(&candidate), Some(0));
		assert!(Member::<T, I>::contains_key(&candidate));
		Ok(())
	}

	#[benchmark]
	fn promote() -> Result<(), BenchmarkError> {
		// Ensure that the `min_promotion_period` wont get in our way.
		let mut params = Params::<T, I>::get();
		let max_rank = T::MaxRank::get();

		// Get minimum promotion period.
		params.min_promotion_period =
			BoundedVec::try_from(vec![Zero::zero(); max_rank as usize]).unwrap();
		Params::<T, I>::put(&params);

		// Start at rank 0 to allow at least one promotion.
		let current_rank = 0;
		let member = make_member::<T, I>(current_rank)?;

		// Set `to_rank` dynamically based on `max_rank`.
		let to_rank = (current_rank + 1).min(max_rank); // Ensure `to_rank` <= `max_rank`.

		// Set it to the max value to ensure that any possible auto-demotion period has passed.
		set_block_number::<T, I>(CoreFellowshipBlockNumberFor::<T, I>::max_value());
		ensure_evidence::<T, I>(&member)?;

		#[extrinsic_call]
		_(RawOrigin::Root, member.clone(), to_rank);

		// Assert the new rank matches `to_rank` (not a hardcoded value).
		assert_eq!(T::Members::rank_of(&member), Some(to_rank));
		assert!(!MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	/// Benchmark the `promote_fast` extrinsic to promote someone up to `r`.
	#[benchmark]
	fn promote_fast(
		r: Linear<1, { ConvertU16ToU32::<T::MaxRank>::get() }>,
	) -> Result<(), BenchmarkError> {
		// Get a target rank for promotion.
		let max_rank = T::MaxRank::get();
		let target_rank = (r as u16).min(max_rank);

		// Begin with Candidate.
		let member = make_member::<T, I>(0)?;

		ensure_evidence::<T, I>(&member)?;

		#[extrinsic_call]
		_(RawOrigin::Root, member.clone(), target_rank);

		assert_eq!(T::Members::rank_of(&member), Some(target_rank));
		assert!(!MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn offboard() -> Result<(), BenchmarkError> {
		let member = make_member::<T, I>(0)?;
		T::Members::demote(&member)?;
		ensure_evidence::<T, I>(&member)?;

		assert!(T::Members::rank_of(&member).is_none());
		assert!(Member::<T, I>::contains_key(&member));
		assert!(MemberEvidence::<T, I>::contains_key(&member));

		#[extrinsic_call]
		_(RawOrigin::Signed(member.clone()), member.clone());

		assert!(!Member::<T, I>::contains_key(&member));
		assert!(!MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn import() -> Result<(), BenchmarkError> {
		let member = account("member", 0, SEED);
		T::Members::induct(&member)?;
		T::Members::promote(&member)?;

		assert!(!Member::<T, I>::contains_key(&member));

		#[extrinsic_call]
		_(RawOrigin::Signed(member.clone()));

		assert!(Member::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn import_member() -> Result<(), BenchmarkError> {
		let member = account("member", 0, SEED);
		let sender = account("sender", 0, SEED);

		T::Members::induct(&member)?;
		T::Members::promote(&member)?;

		assert!(!Member::<T, I>::contains_key(&member));

		#[extrinsic_call]
		_(RawOrigin::Signed(sender), member.clone());

		assert!(Member::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn approve() -> Result<(), BenchmarkError> {
		let member = make_member::<T, I>(1)?;
		let then = T::BlockNumberProvider::current_block_number();
		let now = then.saturating_plus_one();
		set_block_number::<T, I>(now);
		ensure_evidence::<T, I>(&member)?;

		assert_eq!(Member::<T, I>::get(&member).unwrap().last_proof, then);

		#[extrinsic_call]
		_(RawOrigin::Root, member.clone(), 1u8.into());

		assert_eq!(Member::<T, I>::get(&member).unwrap().last_proof, now);
		assert!(!MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	#[benchmark]
	fn submit_evidence() -> Result<(), BenchmarkError> {
		let member = make_member::<T, I>(1)?;
		let evidence = vec![0; Evidence::<T, I>::bound()].try_into().unwrap();

		assert!(!MemberEvidence::<T, I>::contains_key(&member));

		#[extrinsic_call]
		_(RawOrigin::Signed(member.clone()), Wish::Retention, evidence);

		assert!(MemberEvidence::<T, I>::contains_key(&member));
		Ok(())
	}

	impl_benchmark_test_suite! {
		CoreFellowship,
		crate::tests::unit::new_test_ext(),
		crate::tests::unit::Test,
	}
}
