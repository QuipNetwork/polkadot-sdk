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

//! Era reward management.
//!
//! Manages the lifecycle of era reward pot accounts: creation, funding
//! via snapshot from the general DAP pot, and draining of expired eras.
//!
//! Era pots are backed by a rotating pool of `POT_POOL_SIZE` accounts
//! addressed by `era % POT_POOL_SIZE`. Once created, a slot's account is kept
//! alive forever — at the end of each era's history window, its remaining
//! balance is drained to [`crate::Config::UnclaimedRewardHandler`] but the
//! provider reference is retained. A future era that reuses the same slot
//! finds an existing zero-balance account and snapshots into it. This bounds
//! the storage footprint contributed by era pots to a constant.

use crate::*;
use frame_support::{
	defensive,
	traits::{
		fungible::{Balanced, Inspect, Mutate},
		tokens::{Fortitude, Precision, Preservation},
		OnUnbalanced,
	},
};
use sp_runtime::{
	traits::{AtLeast32BitUnsigned, Zero},
	Perbill,
};
use sp_staking::EraIndex;

/// Allocation breakdown of era-end rewards.
#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	scale_info::TypeInfo,
)]
pub struct EraRewardAllocation<Balance> {
	pub staker_rewards: Balance,
	pub validator_incentive: Balance,
}

/// Manager for era reward pot lifecycle.
pub struct EraRewardManager<T: Config>(core::marker::PhantomData<T>);

impl<T: Config> EraRewardManager<T> {
	/// Ensures the era pot account for `(era, kind)` exists by holding a provider
	/// reference. Idempotent: if the slot's account is already provided (because a
	/// previous era reused it), this is a no-op.
	///
	/// Should only be called in non-minting mode (`DisableMinting = true`).
	pub(crate) fn create(era: EraIndex, kind: RewardKind) -> T::AccountId {
		debug_assert!(
			T::DisableMinting::get(),
			"Era pots should only be created when DisableMinting is true"
		);
		let pot_account = T::RewardPots::pot_account(RewardPot::Era(era, kind));
		if frame_system::Pallet::<T>::providers(&pot_account) == 0 {
			frame_system::Pallet::<T>::inc_providers(&pot_account);
		}
		pot_account
	}

	/// Snapshots the general reward pots into era-specific pots.
	///
	/// DAP drips inflation continuously into the general pots. At era boundary,
	/// this transfers the accumulated balances (minus ED) into era pots.
	pub(crate) fn snapshot_era_rewards(era: EraIndex) -> EraRewardAllocation<BalanceOf<T>> {
		let staker_era_pot = Self::create(era, RewardKind::StakerRewards);
		let incentive_era_pot = Self::create(era, RewardKind::ValidatorSelfStake);

		let general_staker_pot =
			T::RewardPots::pot_account(RewardPot::General(RewardKind::StakerRewards));
		let general_incentive_pot =
			T::RewardPots::pot_account(RewardPot::General(RewardKind::ValidatorSelfStake));

		// Leave ED in the general pots to keep them alive.
		let staker_balance = T::Currency::reducible_balance(
			&general_staker_pot,
			Preservation::Preserve,
			Fortitude::Polite,
		);
		let incentive_balance = T::Currency::reducible_balance(
			&general_incentive_pot,
			Preservation::Preserve,
			Fortitude::Polite,
		);

		let actual_staker = if !staker_balance.is_zero() {
			match T::Currency::transfer(
				&general_staker_pot,
				&staker_era_pot,
				staker_balance,
				Preservation::Preserve,
			) {
				Ok(_) => staker_balance,
				Err(e) => {
					log!(error, "Era {:?}: staker reward transfer failed: {:?}", era, e);
					defensive!("Failed to transfer staker rewards to era pot");
					Zero::zero()
				},
			}
		} else {
			Zero::zero()
		};

		let actual_incentive = if !incentive_balance.is_zero() {
			match T::Currency::transfer(
				&general_incentive_pot,
				&incentive_era_pot,
				incentive_balance,
				Preservation::Preserve,
			) {
				Ok(_) => incentive_balance,
				Err(e) => {
					log!(error, "Era {:?}: validator incentive transfer failed: {:?}", era, e);
					defensive!("Failed to transfer validator incentive to era pot");
					Zero::zero()
				},
			}
		} else {
			Zero::zero()
		};

		log!(
			info,
			"Era {:?}: snapshotted staker_rewards={:?}, validator_incentive={:?}",
			era,
			actual_staker,
			actual_incentive
		);

		EraRewardAllocation { staker_rewards: actual_staker, validator_incentive: actual_incentive }
	}

	/// Drains an era pot's remaining balance to the unclaimed reward handler.
	///
	/// The pot account itself is kept alive (provider retained) so the same slot
	/// can be reused by a future era. No-op if the pot was never created (e.g.
	/// the era ran in legacy minting mode).
	pub(crate) fn drain(era: EraIndex, kind: RewardKind) {
		let pot_account = T::RewardPots::pot_account(RewardPot::Era(era, kind));

		// Skip if pot was never created (legacy mode doesn't create pots).
		if frame_system::Pallet::<T>::providers(&pot_account) == 0 {
			return;
		}

		let remaining = T::Currency::balance(&pot_account);

		if remaining.is_zero() {
			return;
		}

		match T::Currency::withdraw(
			&pot_account,
			remaining,
			Precision::BestEffort,
			Preservation::Expendable,
			Fortitude::Force,
		) {
			Ok(credit) => {
				T::UnclaimedRewardHandler::on_unbalanced(credit);
				log!(
					debug,
					"Drained {:?} unclaimed rewards from era {:?} {:?} pot",
					remaining,
					era,
					kind
				);
			},
			Err(e) => {
				defensive!("Failed to withdraw unclaimed rewards from era pot");
				log!(
					error,
					"Era {:?} {:?}: unclaimed reward withdrawal failed: {:?}",
					era,
					kind,
					e
				);
			},
		}
	}

	/// Whether the slot backing this era's staker reward pot exists.
	///
	/// Because slots are reused across eras (rotating pool), this returns
	/// `true` for an era as long as *some* era mapping to the same slot
	/// has created the account.
	#[cfg(any(test, feature = "try-runtime"))]
	pub(crate) fn has_staker_rewards_pot(era: EraIndex) -> bool {
		let pot = T::RewardPots::pot_account(RewardPot::Era(era, RewardKind::StakerRewards));
		frame_system::Pallet::<T>::providers(&pot) > 0
	}

	/// Cleans up all pot accounts for a given era by draining their balances.
	///
	/// Pot accounts are kept alive for reuse by a future era at the same slot.
	pub(crate) fn cleanup_era(era: EraIndex) {
		Self::drain(era, RewardKind::StakerRewards);
		Self::drain(era, RewardKind::ValidatorSelfStake);
	}
}

/// Default implementation of the staker reward calculator.
///
/// Commission-based split: validator gets commission + proportional stake share,
/// nominators get the rest. Incentive weight returns zero (no incentive curve).
pub struct DefaultStakerRewardCalculator<T>(core::marker::PhantomData<T>);

impl<T: Config> sp_staking::StakerRewardCalculator<BalanceOf<T>>
	for DefaultStakerRewardCalculator<T>
where
	BalanceOf<T>: Into<u128> + From<u128>,
{
	fn calculate_validator_incentive_weight(self_stake: BalanceOf<T>) -> BalanceOf<T> {
		let optimum = OptimumSelfStake::<T>::get();
		let cap = HardCapSelfStake::<T>::get();
		let slope_factor = SelfStakeSlopeFactor::<T>::get();

		incentive_weight::<BalanceOf<T>>(self_stake, optimum, cap, slope_factor)
	}

	fn calculate_staker_reward(
		validator_total_reward: BalanceOf<T>,
		validator_commission: Perbill,
		validator_own_stake: BalanceOf<T>,
		total_exposure: BalanceOf<T>,
	) -> sp_staking::StakerRewardResult<BalanceOf<T>> {
		let validator_commission_payout = validator_commission.mul_floor(validator_total_reward);
		let leftover = validator_total_reward.saturating_sub(validator_commission_payout);
		let validator_exposure_part = Perbill::from_rational(validator_own_stake, total_exposure);
		let validator_staking_payout = validator_exposure_part.mul_floor(leftover);
		let validator_payout = validator_staking_payout.saturating_add(validator_commission_payout);
		let nominator_payout = leftover.saturating_sub(validator_staking_payout);

		// Validator and nominator payout is exactly same as total reward.
		debug_assert_eq!(validator_payout + nominator_payout, validator_total_reward);

		sp_staking::StakerRewardResult { validator_payout, nominator_payout }
	}
}

/// Piecewise sqrt-based incentive weight function.
///
/// - Below optimum: `w(s) = √s`
/// - Between optimum and cap: `w(s) = √(T + k² × (s - T))`
/// - Above cap: plateau at `w(cap)`
fn incentive_weight<Balance>(
	self_stake: Balance,
	optimum: Balance,
	cap: Balance,
	slope_factor: Perbill,
) -> Balance
where
	Balance: AtLeast32BitUnsigned + Copy + Into<u128> + From<u128>,
{
	debug_assert!(optimum <= cap, "config invariant: optimum must be <= cap");

	if self_stake.is_zero() {
		return Balance::zero();
	}

	if optimum.is_zero() && cap.is_zero() {
		return Balance::zero();
	}

	let self_stake_u128: u128 = self_stake.into();
	let optimum_u128: u128 = optimum.into();
	let cap_u128: u128 = cap.into();

	let weight_u128 = if self_stake <= optimum {
		sp_arithmetic::helpers_128bit::sqrt(self_stake_u128)
	} else if self_stake <= cap {
		let k_squared = slope_factor.square();
		let excess = self_stake_u128.saturating_sub(optimum_u128);
		let arg = optimum_u128.saturating_add(k_squared.mul_floor(excess));
		sp_arithmetic::helpers_128bit::sqrt(arg)
	} else {
		let k_squared = slope_factor.square();
		let excess = cap_u128.saturating_sub(optimum_u128);
		let arg = optimum_u128.saturating_add(k_squared.mul_floor(excess));
		sp_arithmetic::helpers_128bit::sqrt(arg)
	};

	Balance::from(weight_u128)
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_runtime::Perbill;

	type Balance = u128;

	#[test]
	fn incentive_weight_zero_self_stake() {
		assert_eq!(
			incentive_weight::<Balance>(0, 100_000, 500_000, Perbill::from_rational(1u32, 2u32)),
			0
		);
	}

	#[test]
	fn incentive_weight_config_not_set() {
		// Both optimum and cap are zero (config never set) -> disabled.
		assert_eq!(
			incentive_weight::<Balance>(100_000, 0, 0, Perbill::from_rational(1u32, 2u32)),
			0
		);
	}

	#[test]
	fn incentive_weight_optimum_zero_cap_set() {
		// optimum = 0, cap > 0: dampened-growth zone from 0 up to cap.
		let slope = Perbill::from_rational(1u32, 2u32);
		// self_stake below cap: w(s) = √(0 + 0.25·s) = √(s/4).
		// s = 400_000 -> √100_000 ≈ 316.
		assert_eq!(incentive_weight::<Balance>(400_000, 0, 500_000, slope), 316);
		// Same self-stake with a positive optimum yields higher weight
		assert_eq!(incentive_weight::<Balance>(400_000, 100_000, 500_000, slope), 418);
		// Above cap plateaus at √(0.25·cap) = √125_000 ≈ 353.
		assert_eq!(incentive_weight::<Balance>(1_000_000, 0, 500_000, slope), 353);
	}

	#[test]
	fn incentive_weight_below_optimum() {
		// √10_000 = 100
		assert_eq!(
			incentive_weight::<Balance>(
				10_000,
				100_000,
				500_000,
				Perbill::from_rational(1u32, 2u32)
			),
			100
		);
	}

	#[test]
	fn incentive_weight_at_optimum() {
		// √100_000 ≈ 316
		assert_eq!(
			incentive_weight::<Balance>(
				100_000,
				100_000,
				500_000,
				Perbill::from_rational(1u32, 2u32)
			),
			316
		);
	}

	#[test]
	fn incentive_weight_between_optimum_and_cap() {
		// √(100k + 0.25 × 200k) = √150k ≈ 387
		assert_eq!(
			incentive_weight::<Balance>(
				300_000,
				100_000,
				500_000,
				Perbill::from_rational(1u32, 2u32)
			),
			387
		);
	}

	#[test]
	fn incentive_weight_at_cap() {
		// √(100k + 0.25 × 400k) = √200k ≈ 447
		assert_eq!(
			incentive_weight::<Balance>(
				500_000,
				100_000,
				500_000,
				Perbill::from_rational(1u32, 2u32)
			),
			447
		);
	}

	#[test]
	fn incentive_weight_plateau_above_cap() {
		let at_cap = incentive_weight::<Balance>(
			500_000,
			100_000,
			500_000,
			Perbill::from_rational(1u32, 2u32),
		);
		let above = incentive_weight::<Balance>(
			1_000_000,
			100_000,
			500_000,
			Perbill::from_rational(1u32, 2u32),
		);
		assert_eq!(at_cap, above);
	}

	#[test]
	fn incentive_weight_monotonically_increasing_below_cap() {
		let slope = Perbill::from_rational(1u32, 2u32);
		let w1 = incentive_weight::<Balance>(50_000, 100_000, 500_000, slope);
		let w2 = incentive_weight::<Balance>(100_000, 100_000, 500_000, slope);
		let w3 = incentive_weight::<Balance>(200_000, 100_000, 500_000, slope);
		let w4 = incentive_weight::<Balance>(400_000, 100_000, 500_000, slope);
		assert!(w1 < w2 && w2 < w3 && w3 < w4);
	}

	#[test]
	fn incentive_weight_different_slope_factors() {
		let self_stake = 300_000;
		let w_025 = incentive_weight::<Balance>(
			self_stake,
			100_000,
			500_000,
			Perbill::from_rational(1u32, 4u32),
		);
		let w_050 = incentive_weight::<Balance>(
			self_stake,
			100_000,
			500_000,
			Perbill::from_rational(1u32, 2u32),
		);
		let w_075 = incentive_weight::<Balance>(
			self_stake,
			100_000,
			500_000,
			Perbill::from_rational(3u32, 4u32),
		);
		assert!(w_025 < w_050 && w_050 < w_075);
	}

	#[test]
	fn incentive_weight_slope_factor_zero_plateaus_at_optimum() {
		// k=0 -> immediate plateau at optimum (no growth beyond T).
		let at_optimum = incentive_weight::<Balance>(100_000, 100_000, 500_000, Perbill::zero());
		let above_optimum = incentive_weight::<Balance>(300_000, 100_000, 500_000, Perbill::zero());
		assert_eq!(at_optimum, above_optimum);
	}

	#[test]
	fn incentive_weight_slope_factor_one_no_discouragement() {
		// k=1 -> no discouragement above T (same curve as below T).
		let at_optimum = incentive_weight::<Balance>(100_000, 100_000, 500_000, Perbill::one());
		let at_cap = incentive_weight::<Balance>(500_000, 100_000, 500_000, Perbill::one());
		// sqrt(100_000) = 316, sqrt(500_000) = 707
		assert_eq!(at_optimum, 316);
		assert_eq!(at_cap, 707);
	}

	#[test]
	fn incentive_weight_optimum_equals_cap() {
		// When T == C, the middle segment vanishes -- plateau immediately at T.
		let slope = Perbill::from_rational(1u32, 2u32);
		let at_boundary = incentive_weight::<Balance>(100_000, 100_000, 100_000, slope);
		let above = incentive_weight::<Balance>(200_000, 100_000, 100_000, slope);
		assert_eq!(at_boundary, above);
		assert_eq!(at_boundary, 316); // sqrt(100_000)
	}
}
