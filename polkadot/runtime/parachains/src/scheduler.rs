// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! The scheduler module for parachains and parathreads.
//!
//! This module is responsible for two main tasks:
//!   - Partitioning validators into groups and assigning groups to parachains and parathreads
//!   - Scheduling parachains and parathreads
//!
//! It aims to achieve these tasks with these goals in mind:
//! - It should be possible to know at least a block ahead-of-time, ideally more, which validators
//!   are going to be assigned to which parachains.
//! - Parachains that have a candidate pending availability in this fork of the chain should not be
//!   assigned.
//! - Validator assignments should not be gameable. Malicious cartels should not be able to
//!   manipulate the scheduler to assign themselves as desired.
//! - High or close to optimal throughput of parachains and parathreads. Work among validator groups
//!   should be balanced.
//!
//! The Scheduler manages resource allocation using the concept of "Availability Cores".
//! There will be one availability core for each parachain, and a fixed number of cores
//! used for multiplexing parathreads. Validators will be partitioned into groups, with the same
//! number of groups as availability cores. Validator groups will be assigned to different
//! availability cores over time.

use crate::{configuration, initializer::SessionChangeNotification, paras::AssignCoretime};
use alloc::{
	collections::{btree_map::BTreeMap, vec_deque::VecDeque},
	vec,
	vec::Vec,
};
use frame_support::{pallet_prelude::*, traits::Defensive};
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_primitives::{CoreIndex, GroupIndex, GroupRotationInfo, Id as ParaId, ValidatorIndex};
use sp_runtime::traits::{One, Saturating};

const LOG_TARGET: &str = "runtime::parachains::scheduler";

pub use assigner_coretime::{
	AssignmentState, CoreAssignment, CoreDescriptor, PartsOf57600, QueueDescriptor, Schedule,
	WorkState,
};
pub use pallet::*;
pub use polkadot_core_primitives::v2::BlockNumber;

#[cfg(test)]
mod tests;

/// Implements core assignments as coming from the Coretime chain.
///
/// Depends on the ondemand pallet to assign pool cores.
mod assigner_coretime;

/// Storage migrations for the scheduler pallet.
pub mod migration;

#[frame_support::pallet]
pub mod pallet {

	use crate::on_demand;

	use super::*;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(4);

	#[pallet::pallet]
	#[pallet::without_storage_info]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + configuration::Config + on_demand::Config {}

	#[pallet::error]
	pub enum Error<T> {
		/// assign_core was called with no assignments.
		AssignmentsEmpty,
		/// assign_core with non allowed insertion.
		DisallowedInsert,
	}

	impl<T> From<assigner_coretime::Error> for Error<T> {
		fn from(e: assigner_coretime::Error) -> Self {
			match e {
				assigner_coretime::Error::AssignmentsEmpty => Error::AssignmentsEmpty,
				assigner_coretime::Error::DisallowedInsert => Error::DisallowedInsert,
			}
		}
	}

	/// All the validator groups. One for each core. Indices are into `ActiveValidators` - not the
	/// broader set of Polkadot validators, but instead just the subset used for parachains during
	/// this session.
	///
	/// Bound: The number of cores is the sum of the numbers of parachains and parathread
	/// multiplexers. Reasonably, 100-1000. The dominant factor is the number of validators: safe
	/// upper bound at 10k.
	#[pallet::storage]
	pub type ValidatorGroups<T> = StorageValue<_, Vec<Vec<ValidatorIndex>>, ValueQuery>;

	/// The block number where the session start occurred. Used to track how many group rotations
	/// have occurred.
	///
	/// Note that in the context of parachains modules the session change is signaled during
	/// the block and enacted at the end of the block (at the finalization stage, to be exact).
	/// Thus for all intents and purposes the effect of the session change is observed at the
	/// block following the session change, block number of which we save in this storage value.
	#[pallet::storage]
	pub type SessionStartBlock<T: Config> = StorageValue<_, BlockNumberFor<T>, ValueQuery>;

	/// Scheduled assignment sets for coretime cores.
	///
	/// Assignments as of the given block number. They will go into state once the block number is
	/// reached (and replace whatever was in there before).
	///
	/// Managed by the `assigner_coretime` submodule.
	#[pallet::storage]
	pub type CoreSchedules<T: Config> = StorageMap<
		_,
		Twox64Concat,
		(BlockNumberFor<T>, CoreIndex),
		assigner_coretime::Schedule<BlockNumberFor<T>>,
		OptionQuery,
	>;

	/// Assignments which are currently active for each core.
	///
	/// They will be picked from `CoreSchedules` once we reach the scheduled block number.
	///
	/// Managed by the `assigner_coretime` submodule.
	#[pallet::storage]
	pub type CoreDescriptors<T: Config> = StorageValue<
		_,
		BTreeMap<CoreIndex, assigner_coretime::CoreDescriptor<BlockNumberFor<T>>>,
		ValueQuery,
	>;

	/// Availability timeout status of a core.
	pub(crate) struct AvailabilityTimeoutStatus<BlockNumber> {
		/// Is the core already timed out?
		///
		/// If this is true the core will be freed at this block.
		pub timed_out: bool,

		/// When does this core timeout.
		///
		/// The block number the core times out. If `timed_out` is true, this will correspond to
		/// now (current block number).
		pub live_until: BlockNumber,
	}
}

impl<T: Config> AssignCoretime for Pallet<T> {
	// Only for testing purposes.
	fn assign_coretime(id: ParaId) -> DispatchResult {
		let current_block = frame_system::Pallet::<T>::block_number();

		// Add a new core and assign the para to it.
		let mut config = configuration::ActiveConfig::<T>::get();
		let core = config.scheduler_params.num_cores;
		config.scheduler_params.num_cores.saturating_inc();

		// `assign_coretime` is only called at genesis or by root, so setting the active
		// config here is fine.
		configuration::Pallet::<T>::force_set_active_config(config);

		let begin = current_block + One::one();
		let assignment = vec![(pallet_broker::CoreAssignment::Task(id.into()), PartsOf57600::FULL)];
		assigner_coretime::assign_core::<T>(CoreIndex(core), begin, assignment, None)
			.map_err(Error::<T>::from)?;
		Ok(())
	}
}

impl<T: Config> Pallet<T> {
	/// Assign a particular core ala Coretime.
	pub(crate) fn assign_core(
		core: CoreIndex,
		begin: BlockNumberFor<T>,
		assignment: Vec<(CoreAssignment, PartsOf57600)>,
		end_hint: Option<BlockNumberFor<T>>,
	) -> DispatchResult {
		assigner_coretime::assign_core::<T>(core, begin, assignment, end_hint)
			.map_err(Error::<T>::from)?;
		Ok(())
	}

	/// Advance claim queue.
	///
	/// Parameters:
	/// - is_blocked: Inform whether a given core is currently blocked (schedules can not be
	/// served).
	///
	/// Returns: The `ParaId`s that had been scheduled next, blocked ones are filtered out.
	pub(crate) fn advance_claim_queue<F: Fn(CoreIndex) -> bool>(
		is_blocked: F,
	) -> BTreeMap<CoreIndex, ParaId> {
		let mut assignments = assigner_coretime::advance_assignments::<T, F>(is_blocked);
		assignments.split_off(&CoreIndex(Self::num_availability_cores() as _));
		assignments
	}

	/// Retrieve upcoming claims for each core.
	///
	/// To be called from runtime APIs.
	pub(crate) fn claim_queue() -> BTreeMap<CoreIndex, VecDeque<ParaId>> {
		// Since this is being called from a runtime API, we need to workaround for #64.
		if Self::on_chain_storage_version() == StorageVersion::new(3) {
			return migration::v3::ClaimQueue::<T>::get()
				.into_iter()
				.map(|(core_index, paras)| {
					(core_index, paras.into_iter().map(|e| e.para_id()).collect())
				})
				.collect();
		}

		let config = configuration::ActiveConfig::<T>::get();
		let lookahead = config.scheduler_params.lookahead;
		let mut queue = assigner_coretime::peek_next_block::<T>(lookahead);
		queue.split_off(&CoreIndex(Self::num_availability_cores() as _));
		queue
	}

	/// Called by the initializer to initialize the scheduler pallet.
	pub(crate) fn initializer_initialize(_now: BlockNumberFor<T>) -> Weight {
		Weight::zero()
	}

	/// Called by the initializer to finalize the scheduler pallet.
	pub(crate) fn initializer_finalize() {}

	/// Called by the initializer to note that a new session has started.
	pub(crate) fn initializer_on_new_session(
		notification: &SessionChangeNotification<BlockNumberFor<T>>,
	) {
		let SessionChangeNotification { validators, new_config, .. } = notification;
		let config = new_config;
		let assigner_cores = config.scheduler_params.num_cores;

		let n_cores = core::cmp::max(
			assigner_cores,
			match config.scheduler_params.max_validators_per_core {
				Some(x) if x != 0 => validators.len() as u32 / x,
				_ => 0,
			},
		);

		// shuffle validators into groups.
		if n_cores == 0 || validators.is_empty() {
			ValidatorGroups::<T>::set(Vec::new());
		} else {
			let group_base_size = validators
				.len()
				.checked_div(n_cores as usize)
				.defensive_proof("n_cores should not be 0")
				.unwrap_or(0);
			let n_larger_groups = validators
				.len()
				.checked_rem(n_cores as usize)
				.defensive_proof("n_cores should not be 0")
				.unwrap_or(0);

			// Groups contain indices into the validators from the session change notification,
			// which are already shuffled.

			let mut groups: Vec<Vec<ValidatorIndex>> = Vec::new();
			for i in 0..n_larger_groups {
				let offset = (group_base_size + 1) * i;
				groups.push(
					(0..group_base_size + 1)
						.map(|j| offset + j)
						.map(|j| ValidatorIndex(j as _))
						.collect(),
				);
			}

			for i in 0..(n_cores as usize - n_larger_groups) {
				let offset = (n_larger_groups * (group_base_size + 1)) + (i * group_base_size);
				groups.push(
					(0..group_base_size)
						.map(|j| offset + j)
						.map(|j| ValidatorIndex(j as _))
						.collect(),
				);
			}

			ValidatorGroups::<T>::set(groups);
		}
		let now = frame_system::Pallet::<T>::block_number() + One::one();
		SessionStartBlock::<T>::set(now);
	}

	/// Get the validators in the given group, if the group index is valid for this session.
	pub(crate) fn group_validators(group_index: GroupIndex) -> Option<Vec<ValidatorIndex>> {
		ValidatorGroups::<T>::get().get(group_index.0 as usize).map(|g| g.clone())
	}

	/// Get the number of cores.
	pub(crate) fn num_availability_cores() -> usize {
		ValidatorGroups::<T>::decode_len().unwrap_or(0)
	}

	/// Get the group assigned to a specific core by index at the current block number. Result
	/// undefined if the core index is unknown or the block number is less than the session start
	/// index.
	pub(crate) fn group_assigned_to_core(
		core: CoreIndex,
		at: BlockNumberFor<T>,
	) -> Option<GroupIndex> {
		let config = configuration::ActiveConfig::<T>::get();
		let session_start_block = SessionStartBlock::<T>::get();

		if at < session_start_block {
			return None;
		}

		let validator_groups = ValidatorGroups::<T>::get();

		if core.0 as usize >= validator_groups.len() {
			return None;
		}

		let rotations_since_session_start: BlockNumberFor<T> =
			(at - session_start_block) / config.scheduler_params.group_rotation_frequency;

		let rotations_since_session_start =
			<BlockNumberFor<T> as TryInto<u32>>::try_into(rotations_since_session_start)
				.unwrap_or(0);
		// Error case can only happen if rotations occur only once every u32::max(),
		// so functionally no difference in behavior.

		let group_idx =
			(core.0 as usize + rotations_since_session_start as usize) % validator_groups.len();
		Some(GroupIndex(group_idx as u32))
	}

	/// Returns a predicate that should be used for timing out occupied cores.
	///
	/// This only ever times out cores that have been occupied across a group rotation boundary.
	pub(crate) fn availability_timeout_predicate(
	) -> impl Fn(BlockNumberFor<T>) -> AvailabilityTimeoutStatus<BlockNumberFor<T>> {
		let config = configuration::ActiveConfig::<T>::get();
		let now = frame_system::Pallet::<T>::block_number();
		let rotation_info = Self::group_rotation_info(now);

		let next_rotation = rotation_info.next_rotation_at();

		let times_out = Self::availability_timeout_check_required();

		move |pending_since| {
			let time_out_at = if times_out {
				// We are at the beginning of the rotation, here availability period is relevant.
				// Note: blocks backed in this rotation will never time out here as backed_in +
				// config.paras_availability_period will always be > now for these blocks, as
				// otherwise above condition would not be true.
				pending_since + config.scheduler_params.paras_availability_period
			} else {
				next_rotation + config.scheduler_params.paras_availability_period
			};

			AvailabilityTimeoutStatus { timed_out: time_out_at <= now, live_until: time_out_at }
		}
	}

	/// Is evaluation of `availability_timeout_predicate` necessary at the current block?
	///
	/// This can be used to avoid calling `availability_timeout_predicate` for each core in case
	/// this function returns false.
	pub(crate) fn availability_timeout_check_required() -> bool {
		let config = configuration::ActiveConfig::<T>::get();
		let now = frame_system::Pallet::<T>::block_number() + One::one();
		let rotation_info = Self::group_rotation_info(now);

		let current_window =
			rotation_info.last_rotation_at() + config.scheduler_params.paras_availability_period;
		now < current_window
	}

	/// Returns a helper for determining group rotation.
	pub(crate) fn group_rotation_info(
		now: BlockNumberFor<T>,
	) -> GroupRotationInfo<BlockNumberFor<T>> {
		let session_start_block = SessionStartBlock::<T>::get();
		let group_rotation_frequency = configuration::ActiveConfig::<T>::get()
			.scheduler_params
			.group_rotation_frequency;

		GroupRotationInfo { session_start_block, now, group_rotation_frequency }
	}

	#[cfg(test)]
	fn claim_queue_len() -> usize {
		Self::claim_queue().iter().map(|la_vec| la_vec.1.len()).sum()
	}

	#[cfg(test)]
	#[allow(dead_code)]
	pub(crate) fn claim_queue_is_empty() -> bool {
		Self::claim_queue_len() == 0
	}

	#[cfg(test)]
	pub(crate) fn set_validator_groups(validator_groups: Vec<Vec<ValidatorIndex>>) {
		ValidatorGroups::<T>::set(validator_groups);
	}
}
