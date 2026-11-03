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

//! The parachain coretime assignment module.
//!
//! Handles scheduling of assignments coming from the coretime/broker chain. For on-demand
//! assignments it relies on the separate on-demand pallet, where it forwards requests
//! to.
//!
//! `CoreDescriptor` contains pointers to the begin and the end of a list of schedules, together
//! with the currently active assignments.

mod mock_helpers;
#[cfg(test)]
mod tests;

use crate::{configuration, on_demand, ParaId};

use alloc::{
	collections::{BTreeMap, VecDeque},
	vec::Vec,
};
use frame_support::{defensive, pallet_prelude::*};
use frame_system::pallet_prelude::*;
use polkadot_primitives::CoreIndex;
use scale_info::TypeInfo;
use sp_runtime::{
	codec::{Decode, Encode},
	traits::Saturating,
	Debug,
};

pub use pallet_broker::CoreAssignment;

pub use super::Config;

/// Fraction expressed as a nominator with an assumed denominator of 57,600.
#[derive(
	Debug,
	Clone,
	Copy,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
)]
pub struct PartsOf57600(u16);

impl PartsOf57600 {
	pub const ZERO: Self = Self(0);
	pub const FULL: Self = Self(57600);

	pub fn new_saturating(v: u16) -> Self {
		Self::ZERO.saturating_add(Self(v))
	}

	/// Returns the inner value (test-only accessor).
	#[cfg(test)]
	pub fn value(&self) -> u16 {
		self.0
	}

	pub fn is_full(&self) -> bool {
		*self == Self::FULL
	}

	pub fn saturating_add(self, rhs: Self) -> Self {
		let inner = self.0.saturating_add(rhs.0);
		if inner > 57600 {
			Self(57600)
		} else {
			Self(inner)
		}
	}

	pub fn saturating_sub(self, rhs: Self) -> Self {
		Self(self.0.saturating_sub(rhs.0))
	}

	pub fn checked_add(self, rhs: Self) -> Option<Self> {
		let inner = self.0.saturating_add(rhs.0);
		if inner > 57600 {
			None
		} else {
			Some(Self(inner))
		}
	}
}

/// Assignments as they are scheduled by block number
///
/// for a particular core.
#[derive(Encode, Decode, TypeInfo)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct Schedule<N> {
	/// Original assignments.
	assignments: Vec<(CoreAssignment, PartsOf57600)>,
	/// When do our assignments become invalid, if at all?
	///
	/// If this is `Some`, then this `CoreState` will be dropped at that block number. If this is
	/// `None`, then we will keep serving our core assignments in a circle until a new set of
	/// assignments is scheduled.
	end_hint: Option<N>,

	/// The next queued schedule for this core.
	///
	/// Schedules are forming a queue.
	next_schedule: Option<N>,
}

impl<N> Schedule<N> {
	/// Creates a new Schedule (for tests).
	#[cfg(test)]
	pub fn new(
		assignments: Vec<(CoreAssignment, PartsOf57600)>,
		end_hint: Option<N>,
		next_schedule: Option<N>,
	) -> Self {
		Self { assignments, end_hint, next_schedule }
	}

	/// Accessor for assignments (needed by tests).
	pub fn assignments(&self) -> &[(CoreAssignment, PartsOf57600)] {
		&self.assignments
	}

	/// Accessor for end_hint (needed by tests).
	#[cfg(test)]
	pub fn end_hint(&self) -> Option<N>
	where
		N: Copy,
	{
		self.end_hint
	}

	/// Accessor for next_schedule (needed by migrations, tests, and try-runtime).
	pub(super) fn next_schedule(&self) -> Option<N>
	where
		N: Copy,
	{
		self.next_schedule
	}
}

/// Descriptor for a core.
///
/// Contains pointers to first and last schedule into `CoreSchedules` for that core and keeps track
/// of the currently active work as well.
#[derive(Encode, Decode, TypeInfo, Default)]
#[cfg_attr(test, derive(PartialEq, Debug, Clone))]
pub struct CoreDescriptor<N> {
	/// Meta data about the queued schedules for this core.
	queue: Option<QueueDescriptor<N>>,
	/// Currently performed work.
	current_work: Option<WorkState<N>>,
}

impl<N: PartialOrd> CoreDescriptor<N> {
	/// Creates a new CoreDescriptor (for tests).
	#[cfg(test)]
	pub(super) fn new(
		queue: Option<QueueDescriptor<N>>,
		current_work: Option<WorkState<N>>,
	) -> Self {
		Self { queue, current_work }
	}

	/// Any work currently on that core?
	///
	/// Params: until - until (exclusive) which block number we are interested.
	fn has_assignments_until(&self, until: N) -> bool {
		self.current_work.is_some() || self.queue.as_ref().map_or(false, |q| q.first < until)
	}

	/// Accessor for queue (needed by migrations, tests, and try-runtime).
	pub fn queue(&self) -> Option<&QueueDescriptor<N>> {
		self.queue.as_ref()
	}

	/// Accessor for current_work (needed by migrations, tests, and try-runtime).
	pub fn current_work(&self) -> Option<&WorkState<N>> {
		self.current_work.as_ref()
	}
}

/// Pointers into `CoreSchedules` for a particular core.
///
/// Schedules in `CoreSchedules` form a queue. `Schedule::next_schedule` always pointing to the next
/// item.
#[derive(Encode, Decode, TypeInfo, Copy, Clone)]
#[cfg_attr(test, derive(PartialEq, Debug))]
pub struct QueueDescriptor<N> {
	/// First scheduled item, that is not yet active.
	pub first: N,
	/// Last scheduled item.
	pub last: N,
}

#[derive(Encode, Decode, TypeInfo)]
#[cfg_attr(test, derive(PartialEq, Debug, Clone))]
pub struct WorkState<N> {
	/// Assignments with current state.
	///
	/// Assignments and book keeping on how much has been served already. We keep track of serviced
	/// assignments in order to adhere to the specified ratios.
	pub assignments: Vec<(CoreAssignment, AssignmentState)>,
	/// When do our assignments become invalid if at all?
	///
	/// If this is `Some`, then this `CoreState` will be dropped at that block number. If this is
	/// `None`, then we will keep serving our core assignments in a circle until a new set of
	/// assignments is scheduled.
	pub end_hint: Option<N>,
	/// Position in the assignments we are currently in.
	///
	/// Aka which core assignment will be popped next on
	/// `AssignmentProvider::advance_assignments`.
	pub pos: u16,
	/// Step width
	///
	/// How much we subtract from `AssignmentState::remaining` for a core served.
	pub step: PartsOf57600,
}

#[derive(Encode, Decode, TypeInfo)]
#[cfg_attr(test, derive(PartialEq, Debug, Clone, Copy))]
pub struct AssignmentState {
	/// Ratio of the core this assignment has.
	///
	/// As initially received via `assign_core`.
	pub ratio: PartsOf57600,
	/// How many parts are remaining in this round?
	///
	/// At the end of each round (in preparation for the next), ratio will be added to remaining.
	/// Then every time we get scheduled we subtract a core worth of points. Once we reach 0 or a
	/// number lower than what a core is worth (`CoreState::step` size), we move on to the next
	/// item in the `Vec`.
	///
	/// The first round starts with remaining = ratio.
	pub remaining: PartsOf57600,
}

/// How storage is accessed.
enum AccessMode<'a, T: Config> {
	/// We only want to peek (no side effects).
	Peek { on_demand_orders: &'a mut on_demand::OrderQueue<BlockNumberFor<T>> },
	/// We need to update state.
	Pop,
}

impl<'a, T: Config> AccessMode<'a, T> {
	/// Construct a peeking access mode.
	fn peek(on_demand_orders: &'a mut on_demand::OrderQueue<BlockNumberFor<T>>) -> Self {
		Self::Peek { on_demand_orders }
	}

	/// Construct popping/modifying access mode.
	fn pop() -> Self {
		Self::Pop
	}

	/// Pop pool assignments according to access mode.
	fn pop_assignment_for_ondemand_cores(
		&mut self,
		now: BlockNumberFor<T>,
		num_cores: u32,
	) -> impl Iterator<Item = ParaId> {
		match self {
			Self::Peek { on_demand_orders } => on_demand_orders
				.pop_assignment_for_cores::<T>(now, num_cores)
				.collect::<Vec<_>>(),
			Self::Pop => {
				on_demand::Pallet::<T>::pop_assignment_for_cores(now, num_cores).collect::<Vec<_>>()
			},
		}
		.into_iter()
	}

	/// Get core schedule according to access mode (either take or get).
	fn get_core_schedule(
		&self,
		next_scheduled: BlockNumberFor<T>,
		core_idx: CoreIndex,
	) -> Option<Schedule<BlockNumberFor<T>>> {
		match self {
			Self::Peek { .. } => super::CoreSchedules::<T>::get((next_scheduled, core_idx)),
			Self::Pop => super::CoreSchedules::<T>::take((next_scheduled, core_idx)),
		}
	}
}

/// Assignments that got advanced.
struct AdvancedAssignments {
	bulk_assignments: Vec<(CoreIndex, ParaId)>,
	pool_assignments: Vec<(CoreIndex, ParaId)>,
}

impl AdvancedAssignments {
	fn into_iter(self) -> impl Iterator<Item = (CoreIndex, ParaId)> {
		let Self { bulk_assignments, pool_assignments } = self;
		bulk_assignments.into_iter().chain(pool_assignments.into_iter())
	}
}

impl<N> From<Schedule<N>> for WorkState<N> {
	fn from(schedule: Schedule<N>) -> Self {
		let Schedule { assignments, end_hint, next_schedule: _ } = schedule;
		let step =
			if let Some(min_step_assignment) = assignments.iter().min_by(|a, b| a.1.cmp(&b.1)) {
				min_step_assignment.1
			} else {
				// Assignments empty, should not exist. In any case step size does not matter here:
				log::debug!("assignments of a `Schedule` should never be empty.");
				PartsOf57600(1)
			};
		let assignments = assignments
			.into_iter()
			.map(|(a, ratio)| (a, AssignmentState { ratio, remaining: ratio }))
			.collect();

		Self { assignments, end_hint, pos: 0, step }
	}
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Error {
	AssignmentsEmpty,
	/// assign_core is only allowed to append new assignments at the end of already existing
	/// ones or update the last entry.
	DisallowedInsert,
}

/// Peek `num_entries` into the future.
///
/// First element for each `CoreIndex` will tell what would be retrieved when
/// `advance_assignments` is called at the next block. The second what one would get in the
/// block after the next block and so forth.
///
/// The predictions are accurate in the sense that if an assignment `B` was predicted, it will
/// never happen that `advance_assignments` at that block will retrieve an assignment `A`.
/// What can happen though is that the prediction is empty (returned vec does not contain that
/// element), but `advance_assignments` at that block will then return something regardless.
///
/// Invariants:
///
/// - `advance_assignments` must be called for each core each block
/// exactly once for the prediction offered by `peek_next_block` to stay accurate.
/// - This function is meant to be called from a runtime API and thus uses the state of the
/// block after the current one to show an accurate prediction of upcoming schedules.
pub(super) fn peek_next_block<T: super::Config>(
	num_entries: u32,
) -> BTreeMap<CoreIndex, VecDeque<ParaId>> {
	let now = frame_system::Pallet::<T>::block_number().saturating_plus_one();
	peek_impl::<T>(now, num_entries)
}

/// Advance assignments.
///
/// We move forward one step with the assignments on each core.
///
/// Parameters:
///
/// - blocked: Lambda, for each core it returns true, the assignment could not actually be
/// served.
///
/// Returns: Advanced assignments. Blocked cores will still be advanced, but will not be
/// contained in the output.
pub(super) fn advance_assignments<T: Config, F: Fn(CoreIndex) -> bool>(
	is_blocked: F,
) -> BTreeMap<CoreIndex, ParaId> {
	let now = frame_system::Pallet::<T>::block_number();

	let assignments = super::CoreDescriptors::<T>::mutate(|core_states| {
		advance_assignments_single_impl::<T>(now, core_states, AccessMode::<T>::pop())
	});

	// Give blocked on-demand orders another chance:
	for blocked in assignments.pool_assignments.iter().filter_map(|(core_idx, para_id)| {
		if is_blocked(*core_idx) {
			Some(*para_id)
		} else {
			None
		}
	}) {
		on_demand::Pallet::<T>::push_back_order(blocked);
	}

	let mut assignments: BTreeMap<CoreIndex, ParaId> =
		assignments.into_iter().filter(|(core_idx, _)| !is_blocked(*core_idx)).collect();

	// Try to fill missing assignments from the next position (duplication to allow asynchronous
	// backing even for first assignment coming in on a previously empty core):
	let next = now.saturating_plus_one();
	let mut core_states = super::CoreDescriptors::<T>::get();
	let mut on_demand_orders = on_demand::Pallet::<T>::peek_order_queue();
	let next_assignments = advance_assignments_single_impl(
		next,
		&mut core_states,
		AccessMode::<T>::peek(&mut on_demand_orders),
	)
	.into_iter();

	for (core_idx, next_assignment) in
		next_assignments.filter(|(core_idx, _)| !is_blocked(*core_idx))
	{
		assignments.entry(core_idx).or_insert_with(|| next_assignment);
	}
	assignments
}

/// Append another assignment for a core.
///
/// Important: Only appending is allowed or insertion into the last item. Meaning,
/// all already existing assignments must have a `begin` smaller or equal than the one passed
/// here.
/// Updating the last entry is supported to allow for making a core assignment multiple calls to
/// assign_core. Thus if you have too much interlacing for e.g. a single UMP message you can
/// split that up into multiple messages, each triggering a call to `assign_core`, together
/// forming the total assignment.
///
/// Inserting arbitrarily causes a `DispatchError::DisallowedInsert` error.
///
/// Inserting too early (changing assignments within the lookahead depth), will
/// get the begin auto-adjusted to maintain the stable claim queue invariant, if
/// there existed assignments before.
// With the restriction of only allowing for appends this function allows for
// O(1) complexity. It could easily be lifted, if need be and in fact an
// implementation is available
// [here](https://github.com/paritytech/polkadot-sdk/pull/1694/commits/c0c23b01fd2830910cde92c11960dad12cdff398#diff-0c85a46e448de79a5452395829986ee8747e17a857c27ab624304987d2dde8baR386).
// The problem is that insertion complexity then depends on the size of the existing queue,
// which makes determining weights hard and could lead to issues like overweight blocks (at
// least in theory).
pub(super) fn assign_core<T: Config>(
	core_idx: CoreIndex,
	mut begin: BlockNumberFor<T>,
	mut assignments: Vec<(CoreAssignment, PartsOf57600)>,
	end_hint: Option<BlockNumberFor<T>>,
) -> Result<(), Error> {
	// There should be at least one assignment.
	ensure!(!assignments.is_empty(), Error::AssignmentsEmpty);

	super::CoreDescriptors::<T>::mutate(|core_descriptors| {
		let core_descriptor = core_descriptors.entry(core_idx).or_default();

		let config = configuration::ActiveConfig::<T>::get();
		// Plus 1 because claim queue is always 1 block ahead:
		let now = frame_system::Pallet::<T>::block_number().saturating_plus_one();
		let lookahead = config.scheduler_params.lookahead.into();
		let claim_queue_end = now.saturating_add(lookahead);
		let assignments_exist = core_descriptor.has_assignments_until(claim_queue_end);
		// Maintain invariant of stable claim queue (existing visible assignments not getting
		// replaced):
		if assignments_exist && begin < claim_queue_end {
			log::debug!(
				target: "runtime::parachains::assigner-coretime",
				"Claim queue needs to be stable, schedule change within claim queue length is not supported. Adjusting begin from {:?} to {:?}",
				begin,
				claim_queue_end
			);
			begin = claim_queue_end;
		}

		let new_queue = match core_descriptor.queue {
			Some(queue) => {
				ensure!(begin >= queue.last, Error::DisallowedInsert);

				// Update queue if we are appending:
				if begin > queue.last {
					super::CoreSchedules::<T>::mutate((queue.last, core_idx), |schedule| {
						if let Some(schedule) = schedule.as_mut() {
							debug_assert!(schedule.next_schedule.is_none(), "queue.end was supposed to be the end, so the next item must be `None`!");
							schedule.next_schedule = Some(begin);
						} else {
							defensive!("Queue end entry does not exist?");
						}
					});
				}

				super::CoreSchedules::<T>::mutate((begin, core_idx), |schedule| {
					let assignments = if let Some(mut old_schedule) = schedule.take() {
						old_schedule.assignments.append(&mut assignments);
						old_schedule.assignments
					} else {
						assignments
					};
					*schedule = Some(Schedule { assignments, end_hint, next_schedule: None });
				});

				QueueDescriptor { first: queue.first, last: begin }
			},
			None => {
				// Queue empty, just insert:
				super::CoreSchedules::<T>::insert(
					(begin, core_idx),
					Schedule { assignments, end_hint, next_schedule: None },
				);
				QueueDescriptor { first: begin, last: begin }
			},
		};
		core_descriptor.queue = Some(new_queue);
		Ok(())
	})
}

fn num_coretime_cores<T: Config>() -> u32 {
	configuration::ActiveConfig::<T>::get().scheduler_params.num_cores
}

fn peek_impl<T: Config>(
	mut now: BlockNumberFor<T>,
	num_entries: u32,
) -> BTreeMap<CoreIndex, VecDeque<ParaId>> {
	let mut core_states = super::CoreDescriptors::<T>::get();
	let mut result = BTreeMap::new();
	let mut on_demand_orders = on_demand::Pallet::<T>::peek_order_queue();
	for i in 0..num_entries {
		let assignments = advance_assignments_single_impl(
			now,
			&mut core_states,
			AccessMode::<T>::peek(&mut on_demand_orders),
		)
		.into_iter();
		for (core_idx, para_id) in assignments {
			let claim_queue: &mut VecDeque<ParaId> = result.entry(core_idx).or_default();
			// Stop filling on holes, otherwise we get claims at the wrong positions.
			if claim_queue.len() == i as usize {
				claim_queue.push_back(para_id)
			} else if claim_queue.len() == 0 && i == 1 {
				// Except for position 1: Claim queue was empty before. We now have an incoming
				// assignment on position 1: Duplicate it to position 0 so the chain will
				// get a full asynchronous backing opportunity (and a bonus synchronous
				// backing opportunity).
				claim_queue.push_back(para_id);
				// And fill position 1:
				claim_queue.push_back(para_id);
			}
		}
		now.saturating_inc();
	}
	result
}

/// Pop assignments for `now`.
fn advance_assignments_single_impl<T: Config>(
	now: BlockNumberFor<T>,
	core_states: &mut BTreeMap<CoreIndex, CoreDescriptor<BlockNumberFor<T>>>,
	mut mode: AccessMode<T>,
) -> AdvancedAssignments {
	let mut bulk_assignments = Vec::with_capacity(num_coretime_cores::<T>() as _);
	let mut pool_cores = Vec::with_capacity(num_coretime_cores::<T>() as _);
	for (core_idx, core_state) in core_states.iter_mut() {
		ensure_workload::<T>(now, *core_idx, core_state, &mode);

		let Some(work_state) = core_state.current_work.as_mut() else { continue };

		// Wrap around:
		work_state.pos = work_state.pos % work_state.assignments.len() as u16;
		let (a_type, a_state) = &mut work_state
			.assignments
			.get_mut(work_state.pos as usize)
			.expect("We limited pos to the size of the vec one line above. qed");

		// advance for next pop:
		a_state.remaining = a_state.remaining.saturating_sub(work_state.step);
		if a_state.remaining < work_state.step {
			// Assignment exhausted, need to move to the next and credit remaining for
			// next round.
			work_state.pos += 1;
			// Reset to ratio + still remaining "credits":
			a_state.remaining = a_state.remaining.saturating_add(a_state.ratio);
		}
		match *a_type {
			CoreAssignment::Pool => pool_cores.push(*core_idx),
			CoreAssignment::Task(para_id) => bulk_assignments.push((*core_idx, para_id.into())),
			CoreAssignment::Idle => {},
		}
	}

	let pool_assignments = mode.pop_assignment_for_ondemand_cores(now, pool_cores.len() as _);
	let pool_assignments = pool_cores.into_iter().zip(pool_assignments).collect();

	AdvancedAssignments { bulk_assignments, pool_assignments }
}

/// Ensure given workload for core is up to date.
fn ensure_workload<T: Config>(
	now: BlockNumberFor<T>,
	core_idx: CoreIndex,
	descriptor: &mut CoreDescriptor<BlockNumberFor<T>>,
	mode: &AccessMode<T>,
) {
	// Workload expired?
	if descriptor
		.current_work
		.as_ref()
		.and_then(|w| w.end_hint)
		.map_or(false, |e| e <= now)
	{
		descriptor.current_work = None;
	}

	let Some(queue) = descriptor.queue else {
		// No queue.
		return;
	};

	let mut next_scheduled = queue.first;

	if next_scheduled > now {
		// Not yet ready.
		return;
	}

	// Update is needed:
	let update = loop {
		let Some(update) = mode.get_core_schedule(next_scheduled, core_idx) else { break None };

		// Still good?
		if update.end_hint.map_or(true, |e| e > now) {
			break Some(update);
		}
		// Move on if possible:
		if let Some(n) = update.next_schedule {
			next_scheduled = n;
		} else {
			break None;
		}
	};

	let new_first = update.as_ref().and_then(|u| u.next_schedule);
	descriptor.current_work = update.map(Into::into);

	descriptor.queue = new_first.map(|new_first| {
		QueueDescriptor {
			first: new_first,
			// `last` stays unaffected, if not empty:
			last: queue.last,
		}
	});
}
