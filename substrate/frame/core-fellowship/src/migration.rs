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

//! Storage migrations for `pallet-core-fellowship`.

use super::*;
use frame_support::{
	pallet_prelude::*,
	storage_alias,
	traits::{DefensiveTruncateFrom, UncheckedOnRuntimeUpgrade},
	BoundedVec,
};

#[cfg(feature = "try-runtime")]
use alloc::vec::Vec;
#[cfg(feature = "try-runtime")]
use sp_runtime::TryRuntimeError;

mod v0 {
	use frame_system::pallet_prelude::BlockNumberFor;

	use super::*;

	#[derive(Encode, Decode, Eq, PartialEq, Clone, TypeInfo, MaxEncodedLen, Debug)]
	pub struct ParamsType<Balance, BlockNumber, const RANKS: usize> {
		pub active_salary: [Balance; RANKS],
		pub passive_salary: [Balance; RANKS],
		pub demotion_period: [BlockNumber; RANKS],
		pub min_promotion_period: [BlockNumber; RANKS],
		pub offboard_timeout: BlockNumber,
	}

	impl<Balance: Default + Copy, BlockNumber: Default + Copy, const RANKS: usize> Default
		for ParamsType<Balance, BlockNumber, RANKS>
	{
		fn default() -> Self {
			Self {
				active_salary: [Balance::default(); RANKS],
				passive_salary: [Balance::default(); RANKS],
				demotion_period: [BlockNumber::default(); RANKS],
				min_promotion_period: [BlockNumber::default(); RANKS],
				offboard_timeout: BlockNumber::default(),
			}
		}
	}

	/// Number of available ranks from old version.
	pub(crate) const RANK_COUNT: usize = 9;

	pub type ParamsOf<T, I> = ParamsType<<T as Config<I>>::Balance, BlockNumberFor<T>, RANK_COUNT>;

	/// V0 type for [`crate::Params`].
	#[storage_alias]
	pub type Params<T: Config<I>, I: 'static> =
		StorageValue<Pallet<T, I>, ParamsOf<T, I>, ValueQuery>;
}

pub mod v1 {
	use super::*;
	use frame_system::pallet_prelude::BlockNumberFor as LocalBlockNumberFor;

	pub type MemberStatusOf<T> = MemberStatus<LocalBlockNumberFor<T>>;
	/// V1 type for [`crate::Member`].
	#[storage_alias]
	pub type Member<T: Config<I>, I: 'static> = StorageMap<
		Pallet<T, I>,
		Twox64Concat,
		<T as frame_system::Config>::AccountId,
		MemberStatusOf<T>,
		OptionQuery,
	>;

	pub type ParamsOf<T, I> = ParamsType<
		<T as Config<I>>::Balance,
		LocalBlockNumberFor<T>,
		ConvertU16ToU32<<T as Config<I>>::MaxRank>,
	>;
	/// V1 type for [`crate::Params`].
	#[storage_alias]
	pub type Params<T: Config<I>, I: 'static> =
		StorageValue<Pallet<T, I>, ParamsOf<T, I>, ValueQuery>;

	pub struct MigrateToV1<T, I = ()>(PhantomData<(T, I)>);
	impl<T: Config<I>, I: 'static> UncheckedOnRuntimeUpgrade for MigrateToV1<T, I> {
		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
			ensure!(
				T::MaxRank::get() as usize >= v0::RANK_COUNT,
				"pallet-core-fellowship: new bound should not truncate"
			);
			Ok(Default::default())
		}

		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			// Read the old value from storage
			let old_value = v0::Params::<T, I>::take();
			// Write the new value to storage
			let new = ParamsOf::<T, I> {
				active_salary: BoundedVec::defensive_truncate_from(
					old_value.active_salary.to_vec(),
				),
				passive_salary: BoundedVec::defensive_truncate_from(
					old_value.passive_salary.to_vec(),
				),
				demotion_period: BoundedVec::defensive_truncate_from(
					old_value.demotion_period.to_vec(),
				),
				min_promotion_period: BoundedVec::defensive_truncate_from(
					old_value.min_promotion_period.to_vec(),
				),
				offboard_timeout: old_value.offboard_timeout,
			};
			Params::<T, I>::put(new);
			T::DbWeight::get().reads_writes(1, 1)
		}
	}
}

pub mod v2 {
	use super::*;
	use crate::BlockNumberFor as NewBlockNumberFor;
	use frame_system::pallet_prelude::BlockNumberFor as LocalBlockNumberFor;

	/// Converts previous (local) block number into the new one. May just be identity functions
	/// if sticking with the local block number.
	pub trait ConvertBlockNumber<L, N> {
		/// Converts to the new type and finds the equivalent moment in time as from the view of the
		/// new block provider
		///
		/// # Example usage
		///
		/// ```rust,ignore
		/// // Let's say you are a parachain and switching block providers to the relay chain.
		/// // This will return what the relay block number was at the moment the previous provider's
		/// // number was `local_moment`.
		/// fn equivalent_moment_in_time(local_moment: u32) -> u32 {
		/// 	// How long it's been since 'local_moment' from the parachains pov.
		/// 	let local_block_number = System::block_number();
		/// 	let local_duration = u32::abs_diff(local_block_number, local_moment);
		/// 	// How many blocks that is from the relay's pov.
		/// 	let relay_duration = Self::equivalent_block_duration(local_duration);
		/// 	// What the relay block number must have been at 'local_moment'.
		/// 	let relay_block_number = ParachainSystem::last_relay_block_number();
		/// 	if local_block_number >= local_moment {
		/// 		// Moment was in past.
		/// 		relay_block_number.saturating_sub(relay_duration)
		/// 	} else {
		/// 		// Moment is in future.
		/// 		relay_block_number.saturating_add(relay_duration)
		/// 	}
		/// }
		/// ```
		fn equivalent_moment_in_time(local_moment: L) -> N;

		/// Returns the equivalent number of new blocks it would take to fulfill the same
		/// amount of time in seconds as the old blocks.
		///
		/// For instance - If you previously had 12s blocks and are now following the relay chain's
		/// 6, one local block is equivalent to 2 relay blocks in duration.
		///
		/// # Visualized
		///
		/// ```text
		/// 
		///     6s         6s
		/// |---------||---------|
		///
		///          12s
		/// |--------------------|
		///
		/// ^ Two 6s relay blocks passed per one 12s local block.
		/// ```  
		///
		/// # Example Usage
		///
		/// ```rust,ignore
		/// // Following the scenerio above.
		/// fn equivalent_block_duration(local_duration: u32) -> u32 {
		/// 	local_duration.saturating_mul(2)
		/// }
		/// ```
		fn equivalent_block_duration(local_duration: L) -> N;
	}

	pub struct MigrateToV2<T, BlockNumberConverter, I = ()>(
		PhantomData<(T, BlockNumberConverter, I)>,
	);

	impl<T: Config<I>, BlockNumberConverter, I: 'static> UncheckedOnRuntimeUpgrade
		for MigrateToV2<T, BlockNumberConverter, I>
	where
		BlockNumberConverter: ConvertBlockNumber<LocalBlockNumberFor<T>, NewBlockNumberFor<T, I>>,
	{
		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
			let params_exists = v1::Params::<T, I>::exists();
			let member_count = v1::Member::<T, I>::iter().count() as u32;
			Ok((params_exists, member_count).encode())
		}

		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let mut translation_count = 0;

			// Params conversion
			let old_params = v1::Params::<T, I>::take();
			let new_params = crate::ParamsOf::<T, I> {
				active_salary: old_params.active_salary,
				passive_salary: old_params.passive_salary,
				demotion_period: BoundedVec::defensive_truncate_from(
					old_params
						.demotion_period
						.into_iter()
						.map(|original| BlockNumberConverter::equivalent_block_duration(original))
						.collect(),
				),
				min_promotion_period: BoundedVec::defensive_truncate_from(
					old_params
						.min_promotion_period
						.into_iter()
						.map(|original| BlockNumberConverter::equivalent_block_duration(original))
						.collect(),
				),
				offboard_timeout: BlockNumberConverter::equivalent_block_duration(
					old_params.offboard_timeout,
				),
			};
			crate::Params::<T, I>::put(new_params);
			translation_count.saturating_inc();

			// Member conversion
			crate::Member::<T, I>::translate::<v1::MemberStatusOf<T>, _>(|_, member_data| {
				translation_count.saturating_inc();
				Some(crate::MemberStatus {
					is_active: member_data.is_active,
					last_promotion: BlockNumberConverter::equivalent_moment_in_time(
						member_data.last_promotion,
					),
					last_proof: BlockNumberConverter::equivalent_moment_in_time(
						member_data.last_proof,
					),
				})
			});

			T::DbWeight::get().reads_writes(translation_count, translation_count)
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), TryRuntimeError> {
			let (params_existed, pre_member_count): (bool, u32) =
				Decode::decode(&mut &state[..]).expect("pre_upgrade provides a valid state; qed");

			ensure!(crate::Params::<T, I>::exists() == params_existed, "The Params storage's existence should remain the same before and after the upgrade.");
			let post_member_count = crate::Member::<T, I>::iter().count() as u32;
			ensure!(
				post_member_count == pre_member_count,
				"The member count should remain the same before and after the upgrade."
			);
			Ok(())
		}
	}
}

/// [`UncheckedOnRuntimeUpgrade`] implementation [`MigrateToV1`](v1::MigrateToV1) wrapped in a
/// [`VersionedMigration`](frame_support::migrations::VersionedMigration), which ensures that:
/// - The migration only runs once when the on-chain storage version is 0
/// - The on-chain storage version is updated to `1` after the migration executes
/// - Reads/Writes from checking/settings the on-chain storage version are accounted for
pub type MigrateV0ToV1<T, I> = frame_support::migrations::VersionedMigration<
	0, // The migration will only execute when the on-chain storage version is 0
	1, // The on-chain storage version will be set to 1 after the migration is complete
	v1::MigrateToV1<T, I>,
	crate::pallet::Pallet<T, I>,
	<T as frame_system::Config>::DbWeight,
>;

/// [`UncheckedOnRuntimeUpgrade`] implementation [`MigrateToV2`](v2::MigrateToV2) wrapped in a
/// [`VersionedMigration`](frame_support::migrations::VersionedMigration), which ensures that:
/// - The migration only runs once when the on-chain storage version is `1`.
/// - The on-chain storage version is updated to `2` after the migration executes.
/// - Reads/Writes from checking/settings the on-chain storage version are accounted for.
pub type MigrateV1ToV2<T, BC, I> = frame_support::migrations::VersionedMigration<
	1, // The migration will only execute when the on-chain storage version is 0
	2, // The on-chain storage version will be set to 1 after the migration is complete
	v2::MigrateToV2<T, BC, I>,
	crate::pallet::Pallet<T, I>,
	<T as frame_system::Config>::DbWeight,
>;
