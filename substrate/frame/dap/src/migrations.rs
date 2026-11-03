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

//! DAP pallet migrations.

use super::*;
use crate::weights::WeightInfo;
use frame_support::traits::{Time, UncheckedOnRuntimeUpgrade};

/// V1 to V2 migration: seeds `BudgetAllocation`, credits a one-shot catch-up drip
/// for the window `[P::get(), now]`, and initializes `LastIssuanceTimestamp` to
/// `now` so regular drips start a fresh cadence from here.
///
/// - `T`: DAP pallet config
/// - `P`: `Get<u64>` providing the last inflation timestamp before DAP activation (e.g.
///   `ActiveEra.start` from staking). Only used as an input to the catch-up drip — not persisted.
/// - `B`: `Get<BudgetAllocationMap>` providing the initial budget allocation.
/// - `M`: `Get<u64>` providing the maximum elapsed window (ms) the catch-up is allowed to credit.
///   Should usually be max staking era length.
///
/// Idempotent: the catch-up is skipped if `LastIssuanceTimestamp` is already
/// non-zero, so a re-entry does not double-credit.
///
/// The catch-up drip bypasses `MaxElapsedPerDrip` but is clamped by `M` as the
/// one-shot safety ceiling.
pub type MigrateV1ToV2<T, P, B, M> = frame_support::migrations::VersionedMigration<
	1,
	2,
	InnerMigrateV1ToV2<T, P, B, M>,
	pallet::Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

/// Inner (unversioned) migration logic. Use [`MigrateV1ToV2`] instead.
pub struct InnerMigrateV1ToV2<T, P, B, M>(core::marker::PhantomData<(T, P, B, M)>);

impl<T: Config, P: Get<u64>, B: Get<BudgetAllocationMap>, M: Get<u64>> UncheckedOnRuntimeUpgrade
	for InnerMigrateV1ToV2<T, P, B, M>
{
	fn on_runtime_upgrade() -> frame_support::weights::Weight {
		let mut weight = T::DbWeight::get().reads(3);

		// Seed BudgetAllocation first so the catch-up drip has recipients to distribute to.
		let current_budget = BudgetAllocation::<T>::get();
		if current_budget.is_empty() {
			BudgetAllocation::<T>::put(B::get());
			weight = weight.saturating_add(T::DbWeight::get().writes(1));
			log::info!(target: LOG_TARGET, "Initialized BudgetAllocation with default budget");
		}

		let now: u64 = T::Time::now().saturated_into();

		// Only inflate if `LastIssuanceTimestamp` not set.
		if !LastIssuanceTimestamp::<T>::get().is_zero() {
			log::warn!(
				target: LOG_TARGET,
				"DAP V1->V2: LastIssuanceTimestamp already set; skipping catch-up drip"
			);
			return weight;
		}

		let last_inflation = P::get();
		let raw_elapsed = now.saturating_sub(last_inflation);
		let elapsed = raw_elapsed.min(M::get());
		if elapsed < raw_elapsed {
			log::info!(
				target: LOG_TARGET,
				"DAP V1->V2: elapsed {raw_elapsed}ms clamped to bound {elapsed}ms"
			);
		}
		let minted = pallet::Pallet::<T>::mint_and_distribute(elapsed);
		weight = weight.saturating_add(<T as Config>::WeightInfo::drip_issuance());

		// Regular drips resume from `now`.
		LastIssuanceTimestamp::<T>::put(now);
		weight = weight.saturating_add(T::DbWeight::get().writes(1));

		log::info!(
			target: LOG_TARGET,
			"DAP V1->V2: elapsed={elapsed}ms, total_minted={minted:?}, \
			 seeded LastIssuanceTimestamp={now}"
		);

		weight
	}

	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<alloc::vec::Vec<u8>, sp_runtime::TryRuntimeError> {
		use codec::Encode;

		frame_support::ensure!(
			LastIssuanceTimestamp::<T>::get() == 0 || BudgetAllocation::<T>::get().is_empty(),
			"Migration not needed: LastIssuanceTimestamp and BudgetAllocation already set"
		);

		// Capture `now` and the expected catch-up mint to validate post-upgrade.
		let last_inflation = P::get();
		let now: u64 = T::Time::now().saturated_into();
		let elapsed = now.saturating_sub(last_inflation).min(M::get());
		let total_issuance_before = T::Currency::total_issuance();
		let expected_mint = T::IssuanceCurve::issue(total_issuance_before, elapsed);

		Ok((now, total_issuance_before, expected_mint).encode())
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade(state: alloc::vec::Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
		use codec::Decode;

		let (expected_now, total_issuance_before, expected_mint) =
			<(u64, BalanceOf<T>, BalanceOf<T>)>::decode(&mut &state[..])
				.map_err(|_| "pre_upgrade state decode failed")?;

		// Clock hand-off: regular drips resume from `now`.
		frame_support::ensure!(
			LastIssuanceTimestamp::<T>::get() == expected_now,
			"LastIssuanceTimestamp should equal `now` after migration"
		);

		// Budget invariants (non-empty, registered keys, sum == 100%).
		pallet::Pallet::<T>::do_try_state()?;

		// The catch-up mint should land in `[expected - dust, expected]`. Each recipient
		// share is `Perbill::mul_floor(issuance)`, so the sum only ever rounds *down*,
		// bounded by one unit per budget entry. Anything outside this window indicates
		// something other than the catch-up touched issuance.
		let actual_mint = T::Currency::total_issuance().saturating_sub(total_issuance_before);
		let budget_len = BudgetAllocation::<T>::get().len();
		let max_dust = BalanceOf::<T>::from(budget_len as u32);
		frame_support::ensure!(
			actual_mint <= expected_mint && actual_mint.saturating_add(max_dust) >= expected_mint,
			"Catch-up mint outside expected [expected - dust, expected] window"
		);

		Ok(())
	}
}
