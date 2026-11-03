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

//! Manages all era rotation logic based on session increments.
//!
//! # Lifecycle:
//!
//! When a session ends in RC, a session report is sent to AH with the ending session index. Given
//! there are 6 sessions per Era, and we configure the PlanningEraOffset to be 1, the following
//! happens.
//!
//! ## Idle Sessions
//! In the happy path, first 3 sessions are idle. Nothing much happens in these sessions.
//!
//!
//! ## Planning New Era Session
//! In the happy path, `planning new era` session is initiated when 3rd session ends and the 4th
//! starts in the active era.
//!
//! **Triggers**
//! 1. `SessionProgress == SessionsPerEra - PlanningEraOffset`
//! 2. Forcing is set to `ForceNew` or `ForceAlways`
//!
//! **Actions**
//! 1. Triggers the election process,
//! 2. Updates the CurrentEra.
//!
//! **SkipIf**
//! CurrentEra = ActiveEra + 1 // this implies planning session has already been triggered.
//!
//! **FollowUp**
//! When the election process is over, we send the new validator set, with the CurrentEra index
//! as the id of the validator set.
//!
//!
//! ## Era Rotation Session
//! In the happy path, this happens when the 5th session ends and the 6th starts in the active era.
//!
//! **Triggers**
//! When we receive an activation timestamp from RC.
//!
//! **Assertions**
//! 1. CurrentEra must be ActiveEra + 1.
//! 2. Id of the activation timestamp same as CurrentEra.
//!
//! **Actions**
//! - Finalize the currently active era.
//! - Increment ActiveEra by 1.
//! - Cleanup the old era information.
//!
//! **Exceptional Scenarios**
//! - Delay in exporting validator set: Triggered in a session later than 7th.
//! - Forcing Era: May triggered in a session earlier than 7th.
//!
//! ## Example Flow of a happy path
//!
//! * end 0, start 1, plan 2
//! * end 1, start 2, plan 3
//! * end 2, start 3, plan 4
//! * end 3, start 4, plan 5 // `Plan new era` session. Current Era++. Trigger Election.
//! * **** Somewhere here: Election set is sent to RC, keyed with Current Era
//! * end 4, start 5, plan 6 // RC::session receives and queues this set.
//! * end 5, start 6, plan 7 // Session report contains activation timestamp with Current Era.

use crate::*;
use alloc::{boxed::Box, vec::Vec};
use frame_election_provider_support::{BoundedSupportsOf, ElectionProvider, PageIndex};
use frame_support::{
	pallet_prelude::*,
	traits::{Defensive, DefensiveMax, DefensiveSaturating, OnUnbalanced, TryCollect},
	weights::WeightMeter,
};
use pallet_staking_async_rc_client::RcClientInterface;
use sp_runtime::{Perbill, Percent, Saturating};
use sp_staking::{
	currency_to_vote::CurrencyToVote, Exposure, Page, PagedExposureMetadata, SessionIndex,
	StakerRewardCalculator,
};

/// A handler for all era-based storage items.
///
/// All of the following storage items must be controlled by this type:
///
/// [`ErasValidatorPrefs`]
/// [`ClaimedRewards`]
/// [`ErasStakersPaged`]
/// [`ErasStakersOverview`]
/// [`ErasValidatorReward`]
/// [`ErasRewardPoints`]
/// [`ErasTotalStake`]
pub struct Eras<T: Config>(core::marker::PhantomData<T>);

impl<T: Config> Eras<T> {
	pub(crate) fn set_validator_prefs(era: EraIndex, stash: &T::AccountId, prefs: ValidatorPrefs) {
		debug_assert_eq!(era, Rotator::<T>::planned_era(), "we only set prefs for planning era");
		<ErasValidatorPrefs<T>>::insert(era, stash, prefs);
	}

	pub(crate) fn get_validator_prefs(era: EraIndex, stash: &T::AccountId) -> ValidatorPrefs {
		<ErasValidatorPrefs<T>>::get(era, stash)
	}

	/// Returns validator commission for this era and page.
	pub(crate) fn get_validator_commission(era: EraIndex, stash: &T::AccountId) -> Perbill {
		Self::get_validator_prefs(era, stash).commission
	}

	/// Returns true if the validator has unclaimed pages and earned reward points in the era
	/// (a zero-point payout transfers nothing, so there is nothing to claim).
	pub(crate) fn pending_rewards(era: EraIndex, validator: &T::AccountId) -> bool {
		let Some(overview) = <ErasStakersOverview<T>>::get(&era, validator) else {
			// no exposure, so no rewards to claim.
			return false;
		};

		// Zero reward points means a payout transfers nothing, so there is nothing to claim.
		if Self::get_reward_points_for_validator(era, validator).is_zero() {
			return false;
		}

		ClaimedRewards::<T>::get(era, validator).len() < overview.page_count as usize
	}

	/// Get exposure for a validator at a given era and page.
	///
	/// This is mainly used for rewards and slashing. Validator's self-stake is only returned in
	/// page 0.
	///
	/// This builds a paged exposure from `PagedExposureMetadata` and `ExposurePage` of the
	/// validator.
	pub(crate) fn get_paged_exposure(
		era: EraIndex,
		validator: &T::AccountId,
		page: Page,
	) -> Option<PagedExposure<T::AccountId, BalanceOf<T>>> {
		let overview = <ErasStakersOverview<T>>::get(&era, validator)?;

		// validator stake is added only in page zero.
		let validator_stake = if page == 0 { overview.own } else { Zero::zero() };

		// since overview is present, paged exposure will always be present except when a
		// validator has only own stake and no nominator stake.
		let exposure_page = <ErasStakersPaged<T>>::get((era, validator, page)).unwrap_or_default();

		// build the exposure
		Some(PagedExposure {
			exposure_metadata: PagedExposureMetadata { own: validator_stake, ..overview },
			exposure_page: exposure_page.into(),
		})
	}

	/// Get full exposure of the validator at a given era.
	pub(crate) fn get_full_exposure(
		era: EraIndex,
		validator: &T::AccountId,
	) -> Exposure<T::AccountId, BalanceOf<T>> {
		let Some(overview) = <ErasStakersOverview<T>>::get(&era, validator) else {
			return Exposure::default();
		};

		let mut others = Vec::with_capacity(overview.nominator_count as usize);
		for page in 0..overview.page_count {
			let nominators = <ErasStakersPaged<T>>::get((era, validator, page));
			others.append(&mut nominators.map(|n| n.others.clone()).defensive_unwrap_or_default());
		}

		Exposure { total: overview.total, own: overview.own, others }
	}

	/// Returns the number of pages of exposure a validator has for the given era.
	///
	/// For eras where paged exposure does not exist, this returns 1 to keep backward compatibility.
	pub(crate) fn exposure_page_count(era: EraIndex, validator: &T::AccountId) -> Page {
		<ErasStakersOverview<T>>::get(&era, validator)
			.map(|overview| {
				if overview.page_count == 0 && overview.own > Zero::zero() {
					// Even though there are no nominator pages, there is still validator's own
					// stake exposed which needs to be paid out in a page.
					1
				} else {
					overview.page_count
				}
			})
			// Always returns 1 page for older non-paged exposure.
			// FIXME: Can be cleaned up with issue #13034.
			.unwrap_or(1)
	}

	/// Check whether the validator was exposed at specified era.
	pub(crate) fn was_validator_exposed(era: EraIndex, validator: &T::AccountId) -> bool {
		<ErasStakersOverview<T>>::contains_key(era, validator)
	}

	/// Returns the next page that can be claimed or `None` if nothing to claim.
	pub(crate) fn get_next_claimable_page(era: EraIndex, validator: &T::AccountId) -> Option<Page> {
		// Find next claimable page of paged exposure.
		let page_count = Self::exposure_page_count(era, validator);
		let all_claimable_pages: Vec<Page> = (0..page_count).collect();
		let claimed_pages = ClaimedRewards::<T>::get(era, validator);

		all_claimable_pages.into_iter().find(|p| !claimed_pages.contains(p))
	}

	/// Returns whether nominators are slashable for a specific era.
	///
	/// This checks the per-era storage [`ErasNominatorsSlashable`] which captures
	/// the value of [`AreNominatorsSlashable`] at the start of that era.
	/// If no entry exists for the era, nominators are assumed to be slashable (default).
	pub(crate) fn are_nominators_slashable(era: EraIndex) -> bool {
		ErasNominatorsSlashable::<T>::get(era).unwrap_or(true)
	}

	/// Creates an entry to track validator reward has been claimed for a given era and page.
	/// Noop if already claimed.
	pub(crate) fn set_rewards_as_claimed(era: EraIndex, validator: &T::AccountId, page: Page) {
		let mut claimed_pages = ClaimedRewards::<T>::get(era, validator).into_inner();

		// this should never be called if the reward has already been claimed
		if claimed_pages.contains(&page) {
			defensive!("Trying to set an already claimed reward");
			// nevertheless don't do anything since the page already exist in claimed rewards.
			return;
		}

		// add page to claimed entries
		claimed_pages.push(page);
		ClaimedRewards::<T>::insert(
			era,
			validator,
			WeakBoundedVec::<_, _>::force_from(claimed_pages, Some("set_rewards_as_claimed")),
		);
	}

	/// Store exposure for elected validators at start of an era.
	///
	/// If the exposure does not exist yet for the tuple (era, validator), it sets it. Otherwise,
	/// it updates the existing record by ensuring *intermediate* exposure pages are filled up with
	/// `T::MaxExposurePageSize` number of backers per page and the remaining exposures are added
	/// to new exposure pages.
	pub fn upsert_exposure(
		era: EraIndex,
		validator: &T::AccountId,
		mut exposure: Exposure<T::AccountId, BalanceOf<T>>,
	) {
		let page_size = T::MaxExposurePageSize::get().defensive_max(1);
		if cfg!(debug_assertions) && cfg!(not(feature = "runtime-benchmarks")) {
			// sanitize the exposure in case some test data from this pallet is wrong.
			// ignore benchmarks as other pallets might do weird things.
			let expected_total = exposure
				.others
				.iter()
				.map(|ie| ie.value)
				.fold::<BalanceOf<T>, _>(Default::default(), |acc, x| acc + x)
				.saturating_add(exposure.own);
			debug_assert_eq!(expected_total, exposure.total, "exposure total must equal own + sum(others) for (era: {:?}, validator: {:?}, exposure: {:?})", era, validator, exposure);
		}

		if let Some(overview) = ErasStakersOverview::<T>::get(era, &validator) {
			// collect some info from the un-touched overview for later use.
			let last_page_idx = overview.page_count.saturating_sub(1);
			let mut last_page =
				ErasStakersPaged::<T>::get((era, validator, last_page_idx)).unwrap_or_default();
			let last_page_empty_slots =
				T::MaxExposurePageSize::get().saturating_sub(last_page.others.len() as u32);

			// update nominator-count, page-count, and total stake in overview (done in
			// `update_with`).
			let new_stake_added = exposure.total;
			let new_nominators_added = exposure.others.len() as u32;
			let mut updated_overview = overview
				.update_with::<T::MaxExposurePageSize>(new_stake_added, new_nominators_added);

			// update own stake, if applicable.
			match (updated_overview.own.is_zero(), exposure.own.is_zero()) {
				(true, false) => {
					// first time we see own exposure -- good.
					// note: `total` is already updated above.
					updated_overview.own = exposure.own;
				},
				(true, true) | (false, true) => {
					// no new own exposure is added, nothing to do
				},
				(false, false) => {
					debug_assert!(
						false,
						"validator own stake already set in overview for (era: {:?}, validator: {:?}, current overview: {:?}, new exposure: {:?})",
						era,
						validator,
						updated_overview,
						exposure,
					);
					defensive!("duplicate validator self stake in election");
				},
			};

			ErasStakersOverview::<T>::insert(era, &validator, updated_overview);
			// we are done updating the overview now, `updated_overview` should not be used anymore.
			// We've updated:
			// * nominator count
			// * total stake
			// * own stake (if applicable)
			// * page count
			//
			// next step:
			// * new-keys or updates in `ErasStakersPaged`
			//
			// we don't need the information about own stake anymore -- drop it.
			exposure.total = exposure.total.saturating_sub(exposure.own);
			exposure.own = Zero::zero();

			// splits the exposure so that `append_to_last_page` will fit within the last exposure
			// page, up to the max exposure page size. The remaining individual exposures in
			// `put_in_new_pages` will be added to new pages.
			let append_to_last_page = exposure.split_others(last_page_empty_slots);
			let put_in_new_pages = exposure;

			// handle last page first.

			// fill up last page with exposures.
			last_page.page_total = last_page.page_total.saturating_add(append_to_last_page.total);
			last_page.others.extend(append_to_last_page.others);
			ErasStakersPaged::<T>::insert((era, &validator, last_page_idx), last_page);

			// now handle the remaining exposures and append the exposure pages. The metadata update
			// has been already handled above.
			let (_unused_metadata, put_in_new_pages_chunks) =
				put_in_new_pages.into_pages(page_size);

			put_in_new_pages_chunks
				.into_iter()
				.enumerate()
				.for_each(|(idx, paged_exposure)| {
					let append_at =
						(last_page_idx.saturating_add(1).saturating_add(idx as u32)) as Page;
					<ErasStakersPaged<T>>::insert((era, &validator, append_at), paged_exposure);
				});
		} else {
			// expected page count is the number of nominators divided by the page size, rounded up.
			let expected_page_count = exposure
				.others
				.len()
				.defensive_saturating_add((page_size as usize).defensive_saturating_sub(1))
				.saturating_div(page_size as usize);

			// no exposures yet for this (era, validator) tuple, calculate paged exposure pages and
			// metadata from a blank slate.
			let (exposure_metadata, exposure_pages) = exposure.into_pages(page_size);
			defensive_assert!(exposure_pages.len() == expected_page_count, "unexpected page count");

			// insert metadata.
			ErasStakersOverview::<T>::insert(era, &validator, exposure_metadata);

			// Track that this validator was active in this era for slash liability tracking.
			LastValidatorEra::<T>::insert(validator, era);

			// insert validator's overview.
			exposure_pages.into_iter().enumerate().for_each(|(idx, paged_exposure)| {
				let append_at = idx as Page;
				<ErasStakersPaged<T>>::insert((era, &validator, append_at), paged_exposure);
			});
		};
	}

	pub(crate) fn set_stakers_reward(era: EraIndex, amount: BalanceOf<T>) {
		ErasValidatorReward::<T>::insert(era, amount);
	}

	pub(crate) fn get_stakers_reward(era: EraIndex) -> Option<BalanceOf<T>> {
		ErasValidatorReward::<T>::get(era)
	}

	pub(crate) fn set_validator_incentive_budget(era: EraIndex, amount: BalanceOf<T>) {
		ErasValidatorIncentiveBudget::<T>::insert(era, amount);
	}

	pub(crate) fn get_validator_incentive_budget(era: EraIndex) -> BalanceOf<T> {
		ErasValidatorIncentiveBudget::<T>::get(era)
	}

	pub(crate) fn add_sum_validator_incentive_weight(
		era: EraIndex,
		incentive_weight: BalanceOf<T>,
	) {
		<ErasSumValidatorIncentiveWeight<T>>::mutate(era, |sum| {
			*sum = sum.saturating_add(incentive_weight);
		});
	}

	/// Update the total exposure for all the elected validators in the era.
	pub(crate) fn add_total_stake(era: EraIndex, stake: BalanceOf<T>) {
		<ErasTotalStake<T>>::mutate(era, |total_stake| {
			*total_stake += stake;
		});
	}

	/// Check if the rewards for the given era and page index have been claimed.
	pub(crate) fn is_rewards_claimed(era: EraIndex, validator: &T::AccountId, page: Page) -> bool {
		ClaimedRewards::<T>::get(era, validator).contains(&page)
	}

	/// Add reward points to validators using their stash account ID.
	///
	/// As a side effect, accumulates `weight × points` into [`ErasSumWeightedPoints`] for the
	/// active era, where `weight` is the validator's [`ErasValidatorIncentiveWeight`]. This
	/// keeps the denominator of the weighted-points share up to date without iterating every
	/// validator at payout time.
	pub(crate) fn reward_active_era(
		validators_points: impl IntoIterator<Item = (T::AccountId, u32)>,
	) {
		if let Some(active_era) = ActiveEra::<T>::get() {
			let mut sum_weighted_points_delta: BalanceOf<T> = Zero::zero();
			<ErasRewardPoints<T>>::mutate(active_era.index, |era_rewards| {
				for (validator, points) in validators_points.into_iter() {
					let weight =
						ErasValidatorIncentiveWeight::<T>::get(active_era.index, &validator)
							.unwrap_or_else(Zero::zero);

					let recorded = match era_rewards.individual.get_mut(&validator) {
						Some(individual) => {
							individual.saturating_accrue(points);
							true
						},
						None => {
							// not much we can do -- validators should always be less than
							// `MaxValidatorSet`.
							era_rewards.individual.try_insert(validator, points).defensive().is_ok()
						},
					};

					// Keep the denominator aligned with `individual`, which is the source used
					// by payouts and try-state recomputation. A defensive overflow may leave
					// points unrecorded; those points must not be counted in
					// `ErasSumWeightedPoints`.
					if recorded && !weight.is_zero() {
						sum_weighted_points_delta = sum_weighted_points_delta.saturating_add(
							weight.saturating_mul(IncentiveWeight::<T>::from(points)),
						);
					}

					era_rewards.total.saturating_accrue(points);
				}
			});
			if !sum_weighted_points_delta.is_zero() {
				ErasSumWeightedPoints::<T>::mutate(active_era.index, |sum| {
					*sum = sum.saturating_add(sum_weighted_points_delta);
				});
			}
		}
	}

	pub(crate) fn get_reward_points(era: EraIndex) -> EraRewardPoints<T> {
		ErasRewardPoints::<T>::get(era)
	}

	pub(crate) fn get_reward_points_for_validator(
		era: EraIndex,
		validator: &T::AccountId,
	) -> RewardPoint {
		let points = ErasRewardPoints::<T>::get(era);
		points.individual.get(validator).copied().unwrap_or_default()
	}

	/// Whether era `era` uses the weighted-points incentive-share formula
	/// `share_i = (w_i · ep_i) / Σ_j(w_j · ep_j)`.
	///
	/// Returns `true` for eras at or after [`crate::WeightedPointsFormulaStartEra`], and while
	/// the cutoff is still unset before the migration records it.
	///
	/// Returns `false` for pre-cutoff eras, which fall back to the legacy stake-only share
	/// `share_i = w_i / Σ_j w_j`. Those eras may have reward points credited before their
	/// [`crate::ErasSumWeightedPoints`] denominator was maintained; recomputing it for the full
	/// [`Config::HistoryDepth`] window on upgrade would cost `HistoryDepth × MaxValidatorSet`
	/// reads, so the migration sets the cutoff to `active_era + 1` instead. See
	/// [`crate::migrations::SetWeightedPointsFormulaStartEra`].
	///
	/// Single source of truth for the cutoff decision, shared by the payout path
	/// ([`crate::Pallet::calculate_validator_incentive_for_page`]) and [`Self::do_try_state`].
	pub(crate) fn uses_weighted_points(era: EraIndex) -> bool {
		crate::WeightedPointsFormulaStartEra::<T>::get().map_or(true, |start| era >= start)
	}
}

#[cfg(any(feature = "try-runtime", test, feature = "runtime-benchmarks"))]
#[allow(unused)]
impl<T: Config> Eras<T> {
	/// Ensure the given era's data is fully present (all storage intact and not being pruned).
	pub(crate) fn era_fully_present(era: EraIndex) -> Result<(), sp_runtime::TryRuntimeError> {
		// these two are only set if we have some validators in an era.
		let e0 = ErasValidatorPrefs::<T>::iter_prefix_values(era).count() != 0;
		// note: we don't check `ErasStakersPaged` as a validator can have no backers.
		let e1 = ErasStakersOverview::<T>::iter_prefix_values(era).count() != 0;
		ensure!(e0 == e1, "ErasValidatorPrefs and ErasStakersOverview should be consistent");

		// these two must always be set
		let e2 = ErasTotalStake::<T>::contains_key(era);

		let active_era = Rotator::<T>::active_era();
		let e4 = if era.saturating_sub(1) > 0 &&
			era.saturating_sub(1) > active_era.saturating_sub(T::HistoryDepth::get() + 1)
		{
			// `ErasValidatorReward` is set at active era n for era n-1, and is not set for era 0 in
			// our tests. Moreover, it cannot be checked for presence in the oldest present era
			// (`active_era.saturating_sub(1)`)
			ErasValidatorReward::<T>::contains_key(era.saturating_sub(1))
		} else {
			// ignore
			e2
		};

		ensure!(e2 == e4, "era info presence not consistent");

		if e2 {
			Ok(())
		} else {
			Err("era presence mismatch".into())
		}
	}

	/// Check if the given era is currently being pruned.
	pub(crate) fn era_pruning_in_progress(era: EraIndex) -> bool {
		EraPruningState::<T>::contains_key(era)
	}

	/// Ensure the given era is either absent or currently being pruned.
	pub(crate) fn era_absent_or_pruning(era: EraIndex) -> Result<(), sp_runtime::TryRuntimeError> {
		if Self::era_pruning_in_progress(era) {
			Ok(())
		} else {
			Self::era_absent(era)
		}
	}

	/// Ensure the given era has indeed been already pruned. This is called by the main pallet in
	/// do_prune_era_step.
	pub(crate) fn era_absent(era: EraIndex) -> Result<(), sp_runtime::TryRuntimeError> {
		// check double+ maps
		let e0 = ErasValidatorPrefs::<T>::iter_prefix_values(era).count() != 0;
		let e1 = ErasStakersPaged::<T>::iter_prefix_values((era,)).count() != 0;
		let e2 = ErasStakersOverview::<T>::iter_prefix_values(era).count() != 0;

		// check maps
		// `ErasValidatorReward` is set at active era n for era n-1
		let e3 = ErasValidatorReward::<T>::contains_key(era);
		let e4 = ErasTotalStake::<T>::contains_key(era);

		// these two are only populated conditionally, so we only check them for lack of existence
		let e6 = ClaimedRewards::<T>::iter_prefix_values(era).count() != 0;
		let e7 = ErasRewardPoints::<T>::contains_key(era);

		// Check if era info is consistent - if not, era is in partial pruning state
		if !vec![e0, e1, e2, e3, e4, e6, e7].windows(2).all(|w| w[0] == w[1]) {
			return Err("era info absence not consistent - partial pruning state".into());
		}

		if !e0 {
			Ok(())
		} else {
			Err("era absence mismatch".into())
		}
	}

	pub(crate) fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
		// pruning window works.
		let active_era = Rotator::<T>::active_era();
		// we max with 1 as in active era 0 we don't do an election and therefore we don't have some
		// of the maps populated.
		let oldest_present_era = active_era.saturating_sub(T::HistoryDepth::get()).max(1);

		for e in oldest_present_era..=active_era {
			Self::era_fully_present(e)?;
			Self::check_validator_incentive_weight_consistency(e)?;
			// Eras strictly older than the cutoff use the legacy stake-only formula and may not
			// have `ErasSumWeightedPoints` populated, so skip the denominator consistency check.
			// See
			// [`crate::migrations::SetWeightedPointsFormulaStartEra`].
			if Self::uses_weighted_points(e) {
				Self::check_sum_weighted_points_consistency(e)?;
			}
		}

		// Ensure all eras older than oldest_present_era are either fully pruned or marked for
		// pruning
		ensure!(
			(1..oldest_present_era).all(|e| Self::era_absent_or_pruning(e).is_ok()),
			"All old eras must be either fully pruned or marked for pruning"
		);

		Ok(())
	}

	/// Verify that the sum of individual validator incentive weights matches the stored total.
	fn check_validator_incentive_weight_consistency(
		era: EraIndex,
	) -> Result<(), sp_runtime::TryRuntimeError> {
		use sp_runtime::traits::Zero;

		let stored_total = ErasSumValidatorIncentiveWeight::<T>::get(era);
		let computed_total: BalanceOf<T> = ErasValidatorIncentiveWeight::<T>::iter_prefix(era)
			.fold(BalanceOf::<T>::zero(), |acc, (_, w)| acc.saturating_add(w));

		ensure!(
			stored_total == computed_total,
			"ErasSumValidatorIncentiveWeight mismatch: \
			 stored vs computed individual weights do not match"
		);

		Ok(())
	}

	/// Verify that the incrementally maintained [`ErasSumWeightedPoints`] matches the
	/// recomputed value `Σ_v(weight_v · ep_v)` from current storage.
	fn check_sum_weighted_points_consistency(
		era: EraIndex,
	) -> Result<(), sp_runtime::TryRuntimeError> {
		use sp_runtime::traits::Zero;

		let stored = ErasSumWeightedPoints::<T>::get(era);
		let reward_points = ErasRewardPoints::<T>::get(era);
		let computed: BalanceOf<T> =
			reward_points.individual.iter().fold(BalanceOf::<T>::zero(), |acc, (v, &ep)| {
				let weight =
					ErasValidatorIncentiveWeight::<T>::get(era, v).unwrap_or_else(Zero::zero);
				acc.saturating_add(weight.saturating_mul(BalanceOf::<T>::from(ep)))
			});

		ensure!(
			stored == computed,
			"ErasSumWeightedPoints mismatch: \
			 stored vs computed (Σ weight · era_points) do not match"
		);

		Ok(())
	}
}

/// Manages session rotation logic.
///
/// This controls the following storage items in FULL, meaning that they should not be accessed
/// directly from anywhere else in this pallet:
///
/// * `CurrentEra`: The current planning era
/// * `ActiveEra`: The current active era
/// * `BondedEras`: the list of ACTIVE eras and their session index
pub struct Rotator<T: Config>(core::marker::PhantomData<T>);

impl<T: Config> Rotator<T> {
	#[cfg(feature = "runtime-benchmarks")]
	pub(crate) fn legacy_insta_plan_era() -> Vec<T::AccountId> {
		// Plan the era,
		Self::plan_new_era();
		// signal that we are about to call into elect asap.
		<<T as Config>::ElectionProvider as ElectionProvider>::asap();
		// immediately call into the election provider to fetch and process the results. We assume
		// we are using an instant, onchain election here.
		let msp = <T::ElectionProvider as ElectionProvider>::msp();
		let lsp = 0;
		for p in (lsp..=msp).rev() {
			EraElectionPlanner::<T>::do_elect_paged(p);
		}

		crate::ElectableStashes::<T>::take().into_iter().collect()
	}

	#[cfg(any(feature = "try-runtime", test))]
	pub(crate) fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
		// Check planned era vs active era relationship
		let active_era = ActiveEra::<T>::get();
		let planned_era = CurrentEra::<T>::get();

		let bonded = BondedEras::<T>::get();

		match (&active_era, &planned_era) {
			(None, None) => {
				// Uninitialized state - both should be None
				ensure!(bonded.is_empty(), "BondedEras must be empty when ActiveEra is None");
			},
			(Some(active), Some(planned)) => {
				// Normal state - planned can be at most one more than active
				ensure!(
					*planned == active.index || *planned == active.index + 1,
					"planned era is always equal or one more than active"
				);

				// If we have an active era, bonded eras must always be the range
				// [active - bonding_duration .. active_era]
				let bonded_eras: Vec<_> = bonded.iter().map(|(era, _sess)| *era).collect();
				ensure!(
					bonded_eras ==
						(active.index.saturating_sub(T::BondingDuration::get())..=active.index)
							.collect::<Vec<_>>(),
					"BondedEras range incorrect"
				);

				// ErasNominatorsSlashable entries are cleaned up via lazy pruning at HistoryDepth +
				// 1. Entries can exist from [active - HistoryDepth, active] inclusive.
				// Entries older than HistoryDepth should have been pruned (or be in the process of
				// pruning).
				let oldest_allowed_era = active.index.saturating_sub(T::HistoryDepth::get()).max(1);
				for (era, _) in ErasNominatorsSlashable::<T>::iter() {
					// Allow entries being pruned (EraPruningState exists)
					let being_pruned = EraPruningState::<T>::contains_key(era);
					ensure!(
						(era >= oldest_allowed_era && era <= active.index) || being_pruned,
						"ErasNominatorsSlashable entry exists for era outside history depth range and not being pruned"
					);
				}
			},
			_ => {
				ensure!(false, "ActiveEra and CurrentEra must both be None or both be Some");
			},
		}

		Ok(())
	}

	#[cfg(any(feature = "try-runtime", feature = "std", feature = "runtime-benchmarks", test))]
	pub fn assert_election_ongoing() {
		assert!(Self::is_planning().is_some(), "planning era must exist");
		assert!(
			T::ElectionProvider::status().is_ok(),
			"Election provider must be in a good state during election"
		);
	}

	/// Latest era that was planned.
	///
	/// The returned value does not necessarily indicate that planning for the era with this index
	/// is underway, but rather the last era that was planned. If `Self::active_era()` is equal to
	/// this value, it means that the era is currently active and no new era is planned.
	///
	/// See [`Self::is_planning()`] to only get the next index if planning in progress.
	pub fn planned_era() -> EraIndex {
		CurrentEra::<T>::get().unwrap_or(0)
	}

	pub fn active_era() -> EraIndex {
		ActiveEra::<T>::get().map(|a| a.index).defensive_unwrap_or(0)
	}

	/// Next era that is planned to be started.
	///
	/// Returns None if no era is planned.
	pub fn is_planning() -> Option<EraIndex> {
		let (active, planned) = (Self::active_era(), Self::planned_era());
		if planned.defensive_saturating_sub(active) > 1 {
			defensive!("planned era must always be equal or one more than active");
		}

		(planned > active).then_some(planned)
	}

	/// End the session and start the next one.
	pub(crate) fn end_session(
		end_index: SessionIndex,
		activation_timestamp: Option<(u64, u32)>,
		rewarded_validators: u32,
	) -> Weight {
		// baseline weight for processing the relay chain session report
		let weight = T::WeightInfo::rc_on_session_report(rewarded_validators);

		let Some(active_era) = ActiveEra::<T>::get() else {
			defensive!("Active era must always be available.");
			return weight;
		};
		let current_planned_era = Self::is_planning();
		let starting = end_index + 1;
		// the session after the starting session.
		let planning = starting + 1;

		log!(
			info,
			"Session: end {:?}, start {:?} (ts: {:?}), planning {:?}",
			end_index,
			starting,
			activation_timestamp,
			planning
		);
		log!(info, "Era: active {:?}, planned {:?}", active_era.index, current_planned_era);

		match activation_timestamp {
			Some((time, id)) if Some(id) == current_planned_era => {
				// We rotate the era if we have the activation timestamp.
				Self::start_era(active_era, starting, time);
			},
			Some((_time, id)) => {
				// RC has done something wrong -- we received the wrong ID. Don't start a new era.
				crate::log!(
					warn,
					"received wrong ID with activation timestamp. Got {}, expected {:?}",
					id,
					current_planned_era
				);
				Pallet::<T>::deposit_event(Event::Unexpected(
					UnexpectedKind::UnknownValidatorActivation,
				));
			},
			None => (),
		}

		// check if we should plan new era.
		let should_plan_era = match ForceEra::<T>::get() {
			// see if it's good time to plan a new era.
			Forcing::NotForcing => Self::is_plan_era_deadline(starting),
			// Force plan new era only once.
			Forcing::ForceNew => {
				ForceEra::<T>::put(Forcing::NotForcing);
				true
			},
			// always plan the new era.
			Forcing::ForceAlways => true,
			// never force.
			Forcing::ForceNone => false,
		};

		// Note: we call `planning_era` again, as a new era might have started since we checked
		// it last.
		let has_pending_era = Self::is_planning().is_some();
		match (should_plan_era, has_pending_era) {
			(false, _) => {
				// nothing to consider
			},
			(true, false) => {
				// happy path
				Self::plan_new_era();
			},
			(true, true) => {
				// we are waiting for to start the previously planned era, we cannot plan a new era
				// now.
				crate::log!(
					debug,
					"time to plan a new era {:?}, but waiting for the activation of the previous.",
					current_planned_era
				);
			},
		}

		Pallet::<T>::deposit_event(Event::SessionRotated {
			starting_session: starting,
			active_era: Self::active_era(),
			planned_era: Self::planned_era(),
		});

		weight
	}

	pub(crate) fn start_era(
		ending_era: ActiveEraInfo,
		starting_session: SessionIndex,
		new_era_start_timestamp: u64,
	) {
		// verify that a new era was planned
		debug_assert!(CurrentEra::<T>::get().unwrap_or(0) == ending_era.index + 1);

		let starting_era = ending_era.index + 1;

		// finalize the ending era.
		Self::end_era(&ending_era, new_era_start_timestamp);

		// start the next era.
		Self::start_era_inc_active_era(new_era_start_timestamp);
		Self::start_era_update_bonded_eras(starting_era, starting_session);

		// Snapshot the current nominators slashable setting for this era.
		// Cleanup will happen via lazy pruning at HistoryDepth.
		ErasNominatorsSlashable::<T>::insert(starting_era, AreNominatorsSlashable::<T>::get());

		// cleanup election state
		EraElectionPlanner::<T>::cleanup();

		// Cleanup era pot accounts and mark for lazy pruning.
		if let Some(old_era) = starting_era.checked_sub(T::HistoryDepth::get() + 1) {
			reward::EraRewardManager::<T>::cleanup_era(old_era);
			log!(debug, "Marking era {:?} for lazy pruning", old_era);
			EraPruningState::<T>::insert(old_era, PruningStep::ErasStakersPaged);
		}
	}

	fn start_era_inc_active_era(start_timestamp: u64) {
		ActiveEra::<T>::mutate(|active_era| {
			let new_index = active_era.as_ref().map(|info| info.index + 1).unwrap_or(0);
			log!(
				debug,
				"starting active era {:?} with RC-provided timestamp {:?}",
				new_index,
				start_timestamp
			);
			*active_era = Some(ActiveEraInfo { index: new_index, start: Some(start_timestamp) });
		});
	}

	/// The session index of the current active era.
	///
	/// This must always exist in the `BondedEras` storage item, ergo the function is infallible.
	pub fn active_era_start_session_index() -> SessionIndex {
		Self::era_start_session_index(Self::active_era()).defensive_unwrap_or(0)
	}

	/// The session index of a given era.
	pub fn era_start_session_index(era: EraIndex) -> Option<SessionIndex> {
		BondedEras::<T>::get()
			.into_iter()
			.rev()
			.find_map(|(e, s)| if e == era { Some(s) } else { None })
	}

	fn start_era_update_bonded_eras(starting_era: EraIndex, start_session: SessionIndex) {
		let bonding_duration = T::BondingDuration::get();

		BondedEras::<T>::mutate(|bonded| {
			if bonded.is_full() {
				// remove oldest
				let (era_removed, _) = bonded.remove(0);
				debug_assert!(
					era_removed <= (starting_era.saturating_sub(bonding_duration)),
					"should not delete an era that is not older than bonding duration"
				);
			}

			// must work -- we were not full, or just removed the oldest era.
			let _ = bonded.try_push((starting_era, start_session)).defensive();
		});
	}

	fn end_era(ending_era: &ActiveEraInfo, new_era_start: u64) {
		if T::DisableMinting::get() {
			Self::end_era_dap(ending_era);
		} else {
			Self::end_era_legacy(ending_era, new_era_start);
		}
	}

	/// Legacy end-era: compute inflation via `EraPayout`, mint, send remainder.
	fn end_era_legacy(ending_era: &ActiveEraInfo, new_era_start: u64) {
		let previous_era_start = ending_era.start.defensive_unwrap_or(new_era_start);
		let era_duration = new_era_start.saturating_sub(previous_era_start);

		let cap = T::MaxEraDuration::get();
		let era_duration = if cap == 0 || era_duration <= cap {
			era_duration
		} else {
			Pallet::<T>::deposit_event(Event::Unexpected(UnexpectedKind::EraDurationBoundExceeded));
			log!(
				warn,
				"capping era duration for era {:?} from {:?} to max {:?}",
				ending_era.index,
				era_duration,
				cap
			);
			cap
		};

		let staked = ErasTotalStake::<T>::get(ending_era.index);
		let issuance = asset::total_issuance::<T>();
		let (validator_payout, remainder) =
			T::EraPayout::era_payout(staked, issuance, era_duration);

		let total_payout = validator_payout.saturating_add(remainder);
		let max_staked_rewards = MaxStakedRewards::<T>::get().unwrap_or(Percent::from_percent(100));

		let validator_payout = validator_payout.min(max_staked_rewards * total_payout);
		let remainder = total_payout.saturating_sub(validator_payout);

		Pallet::<T>::deposit_event(Event::<T>::EraPaid {
			era_index: ending_era.index,
			validator_payout,
			remainder,
		});

		Eras::<T>::set_stakers_reward(ending_era.index, validator_payout);
		T::RewardRemainder::on_unbalanced(asset::issue::<T>(remainder));
	}

	/// DAP end-era: snapshot from general reward pots into era-specific pots.
	///
	/// The snapshotted amounts are stored in `ErasValidatorReward` (staker rewards) and
	/// `ErasValidatorIncentiveBudget` (incentive). Individual payouts draw from the era pots.
	fn end_era_dap(ending_era: &ActiveEraInfo) {
		let allocation = reward::EraRewardManager::<T>::snapshot_era_rewards(ending_era.index);

		if allocation.staker_rewards.is_zero() {
			log!(warn, "Era {:?} has zero staker rewards in general pot", ending_era.index);
		}

		Eras::<T>::set_stakers_reward(ending_era.index, allocation.staker_rewards);
		Eras::<T>::set_validator_incentive_budget(ending_era.index, allocation.validator_incentive);

		// Include both staker rewards and validator incentive in the event
		Pallet::<T>::deposit_event(Event::<T>::EraPaid {
			era_index: ending_era.index,
			validator_payout: allocation
				.staker_rewards
				.saturating_add(allocation.validator_incentive),
			remainder: Zero::zero(),
		});

		if DisableMintingGuard::<T>::get().is_none() {
			DisableMintingGuard::<T>::put(ending_era.index);
		}
	}

	/// Plans a new era by kicking off the election process.
	///
	/// The newly planned era is targeted to activate in the next session.
	fn plan_new_era() {
		let _ = CurrentEra::<T>::try_mutate(|x| {
			log!(info, "Planning new era: {:?}, sending election start signal", x.unwrap_or(0));
			let could_start_election = EraElectionPlanner::<T>::plan_new_election();
			*x = Some(x.unwrap_or(0) + 1);
			could_start_election
		});
	}

	/// Returns whether we are at the session where we should plan the new era.
	fn is_plan_era_deadline(start_session: SessionIndex) -> bool {
		let planning_era_offset = T::PlanningEraOffset::get().min(T::SessionsPerEra::get());
		// session at which we should plan the new era.
		let target_plan_era_session = T::SessionsPerEra::get().saturating_sub(planning_era_offset);
		let era_start_session = Self::active_era_start_session_index();

		// progress of the active era in sessions.
		let session_progress = start_session.defensive_saturating_sub(era_start_session);

		log!(
			debug,
			"Session progress within era: {:?}, target_plan_era_session: {:?}",
			session_progress,
			target_plan_era_session
		);
		session_progress >= target_plan_era_session
	}
}

/// Manager type which collects the election results from [`Config::ElectionProvider`] and
/// finalizes the planning of a new era.
///
/// This type managed 3 storage items:
///
/// * [`crate::VoterSnapshotStatus`]
/// * [`crate::NextElectionPage`]
/// * [`crate::ElectableStashes`]
///
/// A new election is fetched over multiple pages, and finalized upon fetching the last page.
///
/// * The intermediate state of fetching the election result is kept in [`NextElectionPage`]. If
///   `Some(_)` something is ongoing, otherwise not.
/// * We fully trust [`Config::ElectionProvider`] to give us a full set of validators, with enough
///   backing after all calls to `maybe_fetch_election_results` are done. Note that older versions
///   of this pallet had a `MinimumValidatorCount` to double-check this, but we don't check it
///   anymore.
/// * `maybe_fetch_election_results` returns a tuple of `(weight, closure)`. The `weight` is the
///   worst-case weight that `exec` might consume. The caller should check if `weight` fits within
///   the boundaries of that context, and execute `closure` if so.
///
/// TODOs:
///
/// * Add a try-state check based on the 3 storage items
/// * Move snapshot creation functions here as well.
pub(crate) struct EraElectionPlanner<T: Config>(PhantomData<T>);
impl<T: Config> EraElectionPlanner<T> {
	/// Cleanup all associated storage items.
	pub(crate) fn cleanup() {
		VoterSnapshotStatus::<T>::kill();
		NextElectionPage::<T>::kill();
		ElectableStashes::<T>::kill();
		Pallet::<T>::register_weight(T::DbWeight::get().writes(3));
	}

	/// Fetches the number of pages configured by the election provider.
	pub(crate) fn election_pages() -> u32 {
		<<T as Config>::ElectionProvider as ElectionProvider>::Pages::get()
	}

	/// Plan a new election
	pub(crate) fn plan_new_election() -> Result<(), <T::ElectionProvider as ElectionProvider>::Error>
	{
		T::ElectionProvider::start()
			.inspect_err(|e| log!(warn, "Election provider failed to start: {:?}", e))
	}

	pub(crate) fn maybe_fetch_election_results() -> (Weight, Box<dyn Fn(&mut WeightMeter)>) {
		let Ok(Some(mut required_weight)) = T::ElectionProvider::status() else {
			// no election ongoing
			let weight = T::DbWeight::get().reads(1);
			return (weight, Box::new(move |meter: &mut WeightMeter| meter.consume(weight)));
		};

		// Add a few things to the required weights that are not captured in `do_elect_paged`, which
		// is benchmarked via `fetch_page`.
		// * 1 extra read and write for `NextElectionPage`
		// * 1 extra write for `RcClientInterface::validator_set` (implementation leak -- we assume
		//   that we know this writes one storage item under the hood)
		// * 1 extra read for `CurrentEra`
		// * 1 extra read for `BondedEras` in `get_prune_up_to`
		// ElectableStashes already read in `do_elect_paged`
		required_weight.saturating_accrue(T::DbWeight::get().reads_writes(3, 2));

		let exec = Box::new(move |meter: &mut WeightMeter| {
			crate::log!(
				debug,
				"Election provider is ready, our status is {:?}",
				NextElectionPage::<T>::get()
			);

			debug_assert!(
				CurrentEra::<T>::get().unwrap_or(0) ==
					ActiveEra::<T>::get().map_or(0, |a| a.index) + 1,
				"Next era must be already planned."
			);

			let current_page = NextElectionPage::<T>::get()
				.unwrap_or(Self::election_pages().defensive_saturating_sub(1));
			let maybe_next_page = current_page.checked_sub(1);
			crate::log!(debug, "fetching page {:?}, next {:?}", current_page, maybe_next_page);

			Self::do_elect_paged(current_page);
			NextElectionPage::<T>::set(maybe_next_page);

			if maybe_next_page.is_none() {
				let id = CurrentEra::<T>::get().defensive_unwrap_or(0);
				let prune_up_to = Self::get_prune_up_to();
				let rc_validators = ElectableStashes::<T>::take().into_iter().collect::<Vec<_>>();

				crate::log!(
					info,
					"Sending new validator set of size {:?} to RC. ID: {:?}, prune_up_to: {:?}",
					rc_validators.len(),
					id,
					prune_up_to
				);
				T::RcClientInterface::validator_set(rc_validators, id, prune_up_to);
			}

			// consume the reported worst case weight.
			meter.consume(required_weight)
		});

		(required_weight, exec)
	}

	/// Get the right value of the first session that needs to be pruned on the RC's historical
	/// session pallet.
	fn get_prune_up_to() -> Option<SessionIndex> {
		let bonded_eras = BondedEras::<T>::get();

		// get the first session of the oldest era in the bonded eras.
		if bonded_eras.is_full() {
			bonded_eras.first().map(|(_, first_session)| first_session.saturating_sub(1))
		} else {
			None
		}
	}

	/// Paginated elect.
	///
	/// Fetches the election page with index `page` from the election provider.
	///
	/// The results from the elect call should be stored in the `ElectableStashes` storage. In
	/// addition, it stores stakers' information for next planned era based on the paged
	/// solution data returned.
	///
	/// If any new election winner does not fit in the electable stashes storage, it truncates
	/// the result of the election. We ensure that only the winners that are part of the
	/// electable stashes have exposures collected for the next era.
	pub(crate) fn do_elect_paged(page: PageIndex) {
		let election_result = T::ElectionProvider::elect(page);
		match election_result {
			Ok(supports) => {
				let inner_processing_results = Self::do_elect_paged_inner(supports);
				if let Err(not_included) = inner_processing_results {
					defensive!(
						"electable stashes exceeded limit, unexpected but election proceeds.\
                		{} stashes from election result discarded",
						not_included
					);
				};

				Pallet::<T>::deposit_event(Event::PagedElectionProceeded {
					page,
					result: inner_processing_results.map(|x| x as u32).map_err(|x| x as u32),
				});
			},
			Err(e) => {
				log!(warn, "election provider page failed due to {:?} (page: {})", e, page);
				Pallet::<T>::deposit_event(Event::PagedElectionProceeded { page, result: Err(0) });
			},
		}
	}

	/// Inner implementation of [`Self::do_elect_paged`].
	///
	/// Returns an error if adding election winners to the electable stashes storage fails due
	/// to exceeded bounds. In case of error, it returns the index of the first stash that
	/// failed to be included.
	pub(crate) fn do_elect_paged_inner(
		mut supports: BoundedSupportsOf<T::ElectionProvider>,
	) -> Result<usize, usize> {
		let planning_era = Rotator::<T>::planned_era();

		match Self::add_electables(supports.iter().map(|(s, _)| s.clone())) {
			Ok(added) => {
				let exposures = Self::collect_exposures(supports);
				let _ = Self::store_stakers_info(exposures, planning_era);
				Ok(added)
			},
			Err(not_included_idx) => {
				let not_included = supports.len().saturating_sub(not_included_idx);

				log!(
					warn,
					"not all winners fit within the electable stashes, excluding {:?} accounts from solution.",
					not_included,
				);

				// filter out supports of stashes that do not fit within the electable stashes
				// storage bounds to prevent collecting their exposures.
				supports.truncate(not_included_idx);
				let exposures = Self::collect_exposures(supports);
				let _ = Self::store_stakers_info(exposures, planning_era);

				Err(not_included)
			},
		}
	}

	/// Process the output of a paged election.
	///
	/// Store staking information for the new planned era of a single election page.
	pub(crate) fn store_stakers_info(
		exposures: BoundedExposuresOf<T>,
		new_planned_era: EraIndex,
	) -> BoundedVec<T::AccountId, MaxWinnersPerPageOf<T::ElectionProvider>> {
		// populate elected stash, stakers, exposures, and the snapshot of validator prefs.
		let mut total_stake_page: BalanceOf<T> = Zero::zero();
		let mut elected_stashes_page = Vec::with_capacity(exposures.len());
		let mut total_backers = 0u32;

		let mut total_incentive_weight_page: BalanceOf<T> = Zero::zero();

		exposures.into_iter().for_each(|(stash, exposure)| {
			log!(
				trace,
				"storing exposure for stash {:?} with {:?} own-stake and {:?} backers",
				stash,
				exposure.own,
				exposure.others.len()
			);
			// build elected stash.
			elected_stashes_page.push(stash.clone());
			// accumulate total stake and backer count for bookkeeping.
			total_stake_page = total_stake_page.saturating_add(exposure.total);
			total_backers += exposure.others.len() as u32;
			let own = exposure.own;
			Eras::<T>::upsert_exposure(new_planned_era, &stash, exposure);

			// Calculate incentive weight from own-stake. Own-stake appears only on the
			// first page of a multi-page exposure, so if the key already exists with a
			// non-zero own, something is wrong upstream.
			if !own.is_zero() {
				if ErasValidatorIncentiveWeight::<T>::contains_key(new_planned_era, &stash) {
					defensive!(
						"validator own-stake seen twice in the same era across election pages"
					);
				} else {
					let incentive_weight =
						T::StakerRewardCalculator::calculate_validator_incentive_weight(own);
					if !incentive_weight.is_zero() {
						total_incentive_weight_page =
							total_incentive_weight_page.saturating_add(incentive_weight);
						ErasValidatorIncentiveWeight::<T>::insert(
							new_planned_era,
							&stash,
							incentive_weight,
						);
					}
				}
			}
		});

		let elected_stashes: BoundedVec<_, MaxWinnersPerPageOf<T::ElectionProvider>> =
			elected_stashes_page
				.try_into()
				.expect("both types are bounded by MaxWinnersPerPageOf; qed");

		// adds to total stake in this era.
		Eras::<T>::add_total_stake(new_planned_era, total_stake_page);

		// adds to total validator self-stake weight for incentive distribution.
		Eras::<T>::add_sum_validator_incentive_weight(new_planned_era, total_incentive_weight_page);

		// collect or update the pref of all winners.
		// TODO: rather inefficient, we can do this once at the last page across all entries in
		// `ElectableStashes`.
		for stash in &elected_stashes {
			let pref = Validators::<T>::get(stash);
			Eras::<T>::set_validator_prefs(new_planned_era, stash, pref);
		}

		log!(
			debug,
			"stored a page of stakers with {:?} validators and {:?} total backers for era {:?}",
			elected_stashes.len(),
			total_backers,
			new_planned_era,
		);

		elected_stashes
	}

	/// Consume a set of [`BoundedSupports`] from [`sp_npos_elections`] and collect them into a
	/// [`Exposure`].
	///
	/// Returns vec of all the exposures of a validator in `paged_supports`, bounded by the
	/// number of max winners per page returned by the election provider.
	fn collect_exposures(
		supports: BoundedSupportsOf<T::ElectionProvider>,
	) -> BoundedExposuresOf<T> {
		let total_issuance = asset::total_issuance::<T>();
		let to_currency = |e: frame_election_provider_support::ExtendedBalance| {
			T::CurrencyToVote::to_currency(e, total_issuance)
		};

		supports
			.into_iter()
			.map(|(validator, support)| {
				// Build `struct exposure` from `support`.
				let mut others = Vec::with_capacity(support.voters.len());
				let mut own: BalanceOf<T> = Zero::zero();
				let mut total: BalanceOf<T> = Zero::zero();
				support
					.voters
					.into_iter()
					.map(|(nominator, weight)| (nominator, to_currency(weight)))
					.for_each(|(nominator, stake)| {
						if nominator == validator {
							defensive_assert!(own == Zero::zero(), "own stake should be unique");
							own = own.saturating_add(stake);
						} else {
							others.push(IndividualExposure { who: nominator, value: stake });
						}
						total = total.saturating_add(stake);
					});

				let exposure = Exposure { own, others, total };
				(validator, exposure)
			})
			.try_collect()
			.expect("we only map through support vector which cannot change the size; qed")
	}

	/// Adds a new set of stashes to the electable stashes.
	///
	/// Returns:
	///
	/// `Ok(newly_added)` if all stashes were added successfully.
	/// `Err(first_un_included)` if some stashes cannot be added due to bounds.
	pub(crate) fn add_electables(
		new_stashes: impl Iterator<Item = T::AccountId>,
	) -> Result<usize, usize> {
		ElectableStashes::<T>::mutate(|electable| {
			let pre_size = electable.len();

			for (idx, stash) in new_stashes.enumerate() {
				if electable.try_insert(stash).is_err() {
					return Err(idx);
				}
			}

			Ok(electable.len() - pre_size)
		})
	}
}
