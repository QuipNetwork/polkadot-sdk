// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Benchmarks for the relayers Pallet.

#![cfg(feature = "runtime-benchmarks")]

use crate::*;

use frame_benchmarking::v2::*;
use frame_support::{assert_ok, weights::Weight};
use frame_system::RawOrigin;
use sp_runtime::traits::One;

/// Reward amount that is (hopefully) is larger than existential deposit across all chains.
const REWARD_AMOUNT: u32 = u32::MAX;

/// Pallet we're benchmarking here.
pub struct Pallet<T: Config<I>, I: 'static = ()>(crate::Pallet<T, I>);

/// Trait that must be implemented by runtime.
pub trait Config<I: 'static = ()>: crate::Config<I> {
	/// `T::Reward` to use in benchmarks.
	fn bench_reward() -> Self::Reward;
	/// Prepare environment for paying the given reward, and optionally return the
	/// `(reward_kind, beneficiary)` pair to use for the `claim_rewards_to`
	/// benchmark. Returning `Some` enables that benchmark and lets the runtime
	/// pick a different reward kind than `bench_reward()` for it (e.g., a
	/// Snowbridge reward routed via XCM to an `AssetHubLocation`, which is not
	/// valid for the basic `claim_rewards` extrinsic). Implementations should
	/// also fund `relayer` with whatever balance the payment path needs (e.g.,
	/// XCM delivery fees).
	///
	/// Returning `None` causes `claim_rewards_to` to be assigned `Weight::MAX`.
	fn prepare_rewards_account(
		relayer: &Self::AccountId,
		reward_kind: Self::Reward,
		reward: Self::RewardBalance,
	) -> Option<(Self::Reward, BeneficiaryOf<Self, I>)>;
	/// Give enough balance to given account.
	fn deposit_account(account: Self::AccountId, balance: Self::Balance);
}

fn assert_last_event<T: Config<I>, I: 'static>(
	generic_event: <T as pallet::Config<I>>::RuntimeEvent,
) {
	frame_system::Pallet::<T>::assert_last_event(generic_event.into());
}

#[instance_benchmarks(
	where
		BeneficiaryOf<T, I>: From<<T as frame_system::Config>::AccountId>,
)]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn claim_rewards() {
		let relayer: T::AccountId = whitelisted_caller();
		let reward_kind = T::bench_reward();
		let reward_balance = T::RewardBalance::from(REWARD_AMOUNT);
		let _ = T::prepare_rewards_account(&relayer, reward_kind, reward_balance);
		RelayerRewards::<T, I>::insert(&relayer, reward_kind, reward_balance);

		#[extrinsic_call]
		_(RawOrigin::Signed(relayer.clone()), reward_kind);

		// we can't check anything here, because `PaymentProcedure` is responsible for
		// payment logic, so we assume that if call has succeeded, the procedure has
		// also completed successfully
		assert_last_event::<T, I>(
			Event::RewardPaid {
				relayer: relayer.clone(),
				reward_kind,
				reward_balance,
				beneficiary: relayer.into(),
			}
			.into(),
		);
	}

	#[benchmark]
	fn claim_rewards_to() -> Result<(), BenchmarkError> {
		let relayer: T::AccountId = whitelisted_caller();
		let reward_balance = T::RewardBalance::from(REWARD_AMOUNT);

		let Some((reward_kind, alternative_beneficiary)) =
			T::prepare_rewards_account(&relayer, T::bench_reward(), reward_balance)
		else {
			return Err(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)));
		};
		RelayerRewards::<T, I>::insert(&relayer, reward_kind, reward_balance);

		#[extrinsic_call]
		_(RawOrigin::Signed(relayer.clone()), reward_kind, alternative_beneficiary.clone());

		// we can't check anything here, because `PaymentProcedure` is responsible for
		// payment logic, so we assume that if call has succeeded, the procedure has
		// also completed successfully
		assert_last_event::<T, I>(
			Event::RewardPaid {
				relayer: relayer.clone(),
				reward_kind,
				reward_balance,
				beneficiary: alternative_beneficiary,
			}
			.into(),
		);

		Ok(())
	}

	#[benchmark]
	fn register() {
		let relayer: T::AccountId = whitelisted_caller();
		let valid_till = frame_system::Pallet::<T>::block_number()
			.saturating_add(crate::Pallet::<T, I>::required_registration_lease())
			.saturating_add(One::one())
			.saturating_add(One::one());
		T::deposit_account(relayer.clone(), crate::Pallet::<T, I>::required_stake());

		#[extrinsic_call]
		_(RawOrigin::Signed(relayer.clone()), valid_till);

		assert!(crate::Pallet::<T, I>::is_registration_active(&relayer));
	}

	#[benchmark]
	fn deregister() {
		let relayer: T::AccountId = whitelisted_caller();
		let valid_till = frame_system::Pallet::<T>::block_number()
			.saturating_add(crate::Pallet::<T, I>::required_registration_lease())
			.saturating_add(One::one())
			.saturating_add(One::one());
		T::deposit_account(relayer.clone(), crate::Pallet::<T, I>::required_stake());
		crate::Pallet::<T, I>::register(RawOrigin::Signed(relayer.clone()).into(), valid_till)
			.unwrap();
		frame_system::Pallet::<T>::set_block_number(valid_till.saturating_add(One::one()));

		#[extrinsic_call]
		_(RawOrigin::Signed(relayer.clone()));

		assert!(!crate::Pallet::<T, I>::is_registration_active(&relayer));
	}

	// Benchmark `slash_and_deregister` method of the pallet. We are adding this weight to
	// the weight of message delivery call if `BridgeRelayersTransactionExtension` signed extension
	// is deployed at runtime level.
	#[benchmark]
	fn slash_and_deregister() {
		// prepare and register relayer account
		let relayer: T::AccountId = whitelisted_caller();
		let valid_till = frame_system::Pallet::<T>::block_number()
			.saturating_add(crate::Pallet::<T, I>::required_registration_lease())
			.saturating_add(One::one())
			.saturating_add(One::one());
		T::deposit_account(relayer.clone(), crate::Pallet::<T, I>::required_stake());
		assert_ok!(crate::Pallet::<T, I>::register(
			RawOrigin::Signed(relayer.clone()).into(),
			valid_till
		));

		// create slash destination account
		let slash_destination: T::AccountId = whitelisted_caller();
		T::deposit_account(slash_destination.clone(), Zero::zero());

		#[block]
		{
			crate::Pallet::<T, I>::slash_and_deregister(
				&relayer,
				bp_relayers::ExplicitOrAccountParams::Explicit::<_, ()>(slash_destination),
			);
		}

		assert!(!crate::Pallet::<T, I>::is_registration_active(&relayer));
	}

	// Benchmark `register_relayer_reward` method of the pallet. We are adding this weight to
	// the weight of message delivery call if `BridgeRelayersTransactionExtension` signed extension
	// is deployed at runtime level.
	#[benchmark]
	fn register_relayer_reward() {
		let reward_kind = T::bench_reward();
		let relayer: T::AccountId = whitelisted_caller();

		#[block]
		{
			crate::Pallet::<T, I>::register_relayer_reward(reward_kind, &relayer, One::one());
		}

		assert_eq!(RelayerRewards::<T, I>::get(relayer, &reward_kind), Some(One::one()));
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::TestRuntime);
}
