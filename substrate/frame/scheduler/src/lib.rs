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

//! > Made with *Substrate*, for *Polkadot*.
//!
//! [![github]](https://github.com/paritytech/polkadot-sdk/tree/master/substrate/frame/scheduler) -
//! [![polkadot]](https://polkadot.com)
//!
//! [polkadot]: https://img.shields.io/badge/polkadot-E6007A?style=for-the-badge&logo=polkadot&logoColor=white
//! [github]: https://img.shields.io/badge/github-8da0cb?style=for-the-badge&labelColor=555555&logo=github
//!
//! # Scheduler Pallet
//!
//! A Pallet for scheduling runtime calls.
//!
//! ## Overview
//!
//! This Pallet exposes capabilities for scheduling runtime calls to occur at a specified block
//! number or at a specified period. These scheduled runtime calls may be named or anonymous and may
//! be canceled.
//!
//! __NOTE:__ Instead of using the filter contained in the origin to call `fn schedule`, scheduled
//! runtime calls will be dispatched with the default filter for the origin: namely
//! `frame_system::Config::BaseCallFilter` for all origin types (except root which will get no
//! filter).
//!
//! If a call is scheduled using proxy or whatever mechanism which adds filter, then those filter
//! will not be used when dispatching the schedule runtime call.
//!
//! ### Examples
//!
//! 1. Scheduling a runtime call at a specific block.
#![doc = docify::embed!("src/tests.rs", basic_scheduling_works)]
//! 2. Scheduling a preimage hash of a runtime call at a specific block
#![doc = docify::embed!("src/tests.rs", scheduling_with_preimages_works)]

//! ## Pallet API
//!
//! See the [`pallet`] module for more information about the interfaces this pallet exposes,
//! including its configuration trait, dispatchables, storage items, events and errors.
//!
//! ## Warning
//!
//! This Pallet executes all scheduled runtime calls in the [`on_initialize`] hook. Do not execute
//! any runtime calls which should not be considered mandatory.
//!
//! Please be aware that any scheduled runtime calls executed in a future block may __fail__ or may
//! result in __undefined behavior__ since the runtime could have upgraded between the time of
//! scheduling and execution. For example, the runtime upgrade could have:
//!
//! * Modified the implementation of the runtime call (runtime specification upgrade).
//!     * Could lead to undefined behavior.
//! * Removed or changed the ordering/index of the runtime call.
//!     * Could fail due to the runtime call index not being part of the `Call`.
//!     * Could lead to undefined behavior, such as executing another runtime call with the same
//!       index.
//!
//! [`on_initialize`]: frame_support::traits::Hooks::on_initialize

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod migration;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
pub mod weights;

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::{borrow::Borrow, cmp::Ordering, marker::PhantomData};
use frame_support::{
	dispatch::{DispatchResult, GetDispatchInfo, Parameter, RawOrigin},
	ensure,
	traits::{
		schedule::{self, DispatchTime, MaybeHashed},
		Bounded, CallerTrait, EnsureOrigin, Get, IsType, OriginTrait, PalletInfoAccess,
		PrivilegeCmp, QueryPreimage, StorageVersion, StorePreimage,
	},
	weights::{Weight, WeightMeter},
};
use frame_system::{self as system};
use scale_info::TypeInfo;
use sp_io::hashing::blake2_256;
use sp_runtime::{
	traits::{BadOrigin, BlockNumberProvider, Dispatchable, One, Saturating, Zero},
	BoundedVec, Debug, DispatchError,
};

pub use pallet::*;
pub use weights::WeightInfo;

/// Just a simple index for naming period tasks.
pub type PeriodicIndex = u32;
/// The location of a scheduled task that can be used to remove it.
pub type TaskAddress<BlockNumber> = (BlockNumber, u32);

pub type CallOrHashOf<T> =
	MaybeHashed<<T as Config>::RuntimeCall, <T as frame_system::Config>::Hash>;

pub type BoundedCallOf<T> =
	Bounded<<T as Config>::RuntimeCall, <T as frame_system::Config>::Hashing>;

pub type BlockNumberFor<T> =
	<<T as Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

/// The configuration of the retry mechanism for a given task along with its current state.
#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
)]
pub struct RetryConfig<Period> {
	/// Initial amount of retries allowed.
	pub total_retries: u8,
	/// Amount of retries left.
	pub remaining: u8,
	/// Period of time between retry attempts.
	pub period: Period,
}

#[cfg_attr(any(feature = "std", test), derive(PartialEq, Eq))]
#[derive(Clone, Debug, Encode, Decode)]
struct ScheduledV1<Call, BlockNumber> {
	maybe_id: Option<Vec<u8>>,
	priority: schedule::Priority,
	call: Call,
	maybe_periodic: Option<schedule::Period<BlockNumber>>,
}

/// Information regarding an item to be executed in the future.
#[derive(
	Clone, Debug, PartialEq, Eq, Encode, Decode, MaxEncodedLen, TypeInfo, DecodeWithMemTracking,
)]
pub struct Scheduled<Name, Call, BlockNumber, PalletsOrigin, AccountId> {
	/// The unique identity for this task, if there is one.
	pub maybe_id: Option<Name>,
	/// This task's priority.
	pub priority: schedule::Priority,
	/// The call to be dispatched.
	pub call: Call,
	/// If the call is periodic, then this points to the information concerning that.
	pub maybe_periodic: Option<schedule::Period<BlockNumber>>,
	/// The origin with which to dispatch the call.
	pub origin: PalletsOrigin,
	#[doc(hidden)]
	pub _phantom: PhantomData<AccountId>,
}

impl<Name, Call, BlockNumber, PalletsOrigin, AccountId>
	Scheduled<Name, Call, BlockNumber, PalletsOrigin, AccountId>
where
	Call: Clone,
	PalletsOrigin: Clone,
{
	/// Create a new task to be used for retry attempts of the original one. The cloned task will
	/// have the same `priority`, `call` and `origin`, but will always be non-periodic and unnamed.
	pub fn as_retry(&self) -> Self {
		Self {
			maybe_id: None,
			priority: self.priority,
			call: self.call.clone(),
			maybe_periodic: None,
			origin: self.origin.clone(),
			_phantom: Default::default(),
		}
	}
}

use crate::{Scheduled as ScheduledV3, Scheduled as ScheduledV2};

pub type ScheduledV2Of<T> = ScheduledV2<
	Vec<u8>,
	<T as Config>::RuntimeCall,
	BlockNumberFor<T>,
	<T as Config>::PalletsOrigin,
	<T as frame_system::Config>::AccountId,
>;

pub type ScheduledV3Of<T> = ScheduledV3<
	Vec<u8>,
	CallOrHashOf<T>,
	BlockNumberFor<T>,
	<T as Config>::PalletsOrigin,
	<T as frame_system::Config>::AccountId,
>;

pub type ScheduledOf<T> = Scheduled<
	TaskName,
	BoundedCallOf<T>,
	BlockNumberFor<T>,
	<T as Config>::PalletsOrigin,
	<T as frame_system::Config>::AccountId,
>;

pub(crate) trait MarginalWeightInfo: WeightInfo {
	fn service_task(maybe_lookup_len: Option<usize>, named: bool, periodic: bool) -> Weight {
		let base = Self::service_task_base();
		let mut total = match maybe_lookup_len {
			None => base,
			Some(l) => Self::service_task_fetched(l as u32),
		};
		if named {
			total.saturating_accrue(Self::service_task_named().saturating_sub(base));
		}
		if periodic {
			total.saturating_accrue(Self::service_task_periodic().saturating_sub(base));
		}
		total
	}
}
impl<T: WeightInfo> MarginalWeightInfo for T {}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::{dispatch::PostDispatchInfo, pallet_prelude::*};
	use frame_system::pallet_prelude::{BlockNumberFor as SystemBlockNumberFor, OriginFor};

	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(4);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	/// `system::Config` should always be included in our implied traits.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The aggregated origin which the dispatch will take.
		type RuntimeOrigin: OriginTrait<PalletsOrigin = Self::PalletsOrigin>
			+ From<Self::PalletsOrigin>
			+ IsType<<Self as system::Config>::RuntimeOrigin>;

		/// The caller origin, overarching type of all pallets origins.
		type PalletsOrigin: From<system::RawOrigin<Self::AccountId>>
			+ CallerTrait<Self::AccountId>
			+ MaxEncodedLen;

		/// The aggregated call type.
		type RuntimeCall: Parameter
			+ Dispatchable<
				RuntimeOrigin = <Self as Config>::RuntimeOrigin,
				PostInfo = PostDispatchInfo,
			> + GetDispatchInfo
			+ From<system::Call<Self>>;

		/// The maximum weight that may be scheduled per block for any dispatchables.
		#[pallet::constant]
		type MaximumWeight: Get<Weight>;

		/// Required origin to schedule or cancel calls.
		type ScheduleOrigin: EnsureOrigin<<Self as system::Config>::RuntimeOrigin>;

		/// Compare the privileges of origins.
		///
		/// This will be used when canceling a task, to ensure that the origin that tries
		/// to cancel has greater or equal privileges as the origin that created the scheduled task.
		///
		/// For simplicity the [`EqualPrivilegeOnly`](frame_support::traits::EqualPrivilegeOnly) can
		/// be used. This will only check if two given origins are equal.
		type OriginPrivilegeCmp: PrivilegeCmp<Self::PalletsOrigin>;

		/// The maximum number of scheduled calls in the queue for a single block.
		///
		/// NOTE:
		/// + Dependent pallets' benchmarks might require a higher limit for the setting. Set a
		/// higher limit under `runtime-benchmarks` feature.
		#[pallet::constant]
		type MaxScheduledPerBlock: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// The preimage provider with which we look up call hashes to get the call.
		type Preimages: QueryPreimage<H = Self::Hashing> + StorePreimage;

		/// Query the current block number.
		///
		/// Must return monotonically increasing values when called from consecutive blocks. It is
		/// generally expected that the values also do not differ "too much" between consecutive
		/// blocks. A future addition to this pallet will allow bigger difference between
		/// consecutive blocks to make it possible to be utilized by parachains with *Agile
		/// Coretime*. *Agile Coretime* parachains are currently not supported and must continue to
		/// use their local block number provider.
		///
		/// Can be configured to return either:
		/// - the local block number of the runtime via `frame_system::Pallet`
		/// - a remote block number, eg from the relay chain through `RelaychainDataProvider`
		/// - an arbitrary value through a custom implementation of the trait
		///
		/// Suggested values:
		/// - Solo- and Relay-chains should use `frame_system::Pallet`. There are no concerns with
		///   this configuration.
		/// - Parachains should also use `frame_system::Pallet` for the time being. The scheduler
		///   pallet is not yet ready for the case that big numbers of blocks are skipped. In an
		///   *Agile Coretime* chain with relay chain number provider configured, it could otherwise
		///   happen that the scheduler will not be able to catch up to its agendas, since too many
		///   relay blocks are missing if the parachain only produces blocks rarely.
		///
		/// There is currently no migration provided to "hot-swap" block number providers and it is
		/// therefore highly advised to stay with the default (local) values. If you still want to
		/// swap block number providers on the fly, then please at least ensure that you do not run
		/// any pallet migration in the same runtime upgrade.
		type BlockNumberProvider: BlockNumberProvider;
	}

	/// Block number at which the agenda began incomplete execution.
	#[pallet::storage]
	pub type IncompleteSince<T: Config> = StorageValue<_, BlockNumberFor<T>>;

	/// Items to be executed, indexed by the block number that they should be executed on.
	#[pallet::storage]
	pub type Agenda<T: Config> = StorageMap<
		_,
		Twox64Concat,
		BlockNumberFor<T>,
		BoundedVec<Option<ScheduledOf<T>>, T::MaxScheduledPerBlock>,
		ValueQuery,
	>;

	/// Retry configurations for items to be executed, indexed by task address.
	#[pallet::storage]
	pub type Retries<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		TaskAddress<BlockNumberFor<T>>,
		RetryConfig<BlockNumberFor<T>>,
		OptionQuery,
	>;

	/// Lookup from a name to the block number and index of the task.
	///
	/// For v3 -> v4 the previously unbounded identities are Blake2-256 hashed to form the v4
	/// identities.
	#[pallet::storage]
	pub type Lookup<T: Config> =
		StorageMap<_, Twox64Concat, TaskName, TaskAddress<BlockNumberFor<T>>>;

	/// Events type.
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Scheduled some task.
		Scheduled { when: BlockNumberFor<T>, index: u32 },
		/// Canceled some task.
		Canceled { when: BlockNumberFor<T>, index: u32 },
		/// Dispatched some task.
		Dispatched {
			task: TaskAddress<BlockNumberFor<T>>,
			id: Option<TaskName>,
			result: DispatchResult,
		},
		/// Set a retry configuration for some task.
		RetrySet {
			task: TaskAddress<BlockNumberFor<T>>,
			id: Option<TaskName>,
			period: BlockNumberFor<T>,
			retries: u8,
		},
		/// Cancel a retry configuration for some task.
		RetryCancelled { task: TaskAddress<BlockNumberFor<T>>, id: Option<TaskName> },
		/// The call for the provided hash was not found so the task has been aborted.
		CallUnavailable { task: TaskAddress<BlockNumberFor<T>>, id: Option<TaskName> },
		/// The given task was unable to be renewed since the agenda is full at that block.
		PeriodicFailed { task: TaskAddress<BlockNumberFor<T>>, id: Option<TaskName> },
		/// The given task was unable to be retried since the agenda is full at that block or there
		/// was not enough weight to reschedule it.
		RetryFailed { task: TaskAddress<BlockNumberFor<T>>, id: Option<TaskName> },
		/// The given task can never be executed since it is overweight.
		PermanentlyOverweight { task: TaskAddress<BlockNumberFor<T>>, id: Option<TaskName> },
		/// Agenda is incomplete from `when`.
		AgendaIncomplete { when: BlockNumberFor<T> },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Failed to schedule a call
		FailedToSchedule,
		/// Cannot find the scheduled call.
		NotFound,
		/// Given target block number is in the past.
		TargetBlockNumberInPast,
		/// Reschedule failed because it does not change scheduled time.
		RescheduleNoChange,
		/// Attempt to use a non-named function on a named task.
		Named,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<SystemBlockNumberFor<T>> for Pallet<T> {
		/// Execute the scheduled calls
		fn on_initialize(_now: SystemBlockNumberFor<T>) -> Weight {
			let now = T::BlockNumberProvider::current_block_number();
			let mut weight_counter = frame_system::Pallet::<T>::remaining_block_weight()
				.limit_to(T::MaximumWeight::get());
			Self::service_agendas(&mut weight_counter, now, u32::MAX);
			weight_counter.consumed()
		}

		#[cfg(feature = "std")]
		fn integrity_test() {
			/// Calculate the maximum weight that a lookup of a given size can take.
			fn lookup_weight<T: Config>(s: usize) -> Weight {
				T::WeightInfo::service_agendas_base() +
					T::WeightInfo::service_agenda_base(T::MaxScheduledPerBlock::get()) +
					T::WeightInfo::service_task(Some(s), true, true)
			}

			let limit = sp_runtime::Perbill::from_percent(90) * T::MaximumWeight::get();

			let small_lookup = lookup_weight::<T>(128);
			assert!(small_lookup.all_lte(limit), "Must be possible to submit a small lookup");

			let medium_lookup = lookup_weight::<T>(1024);
			assert!(medium_lookup.all_lte(limit), "Must be possible to submit a medium lookup");

			let large_lookup = lookup_weight::<T>(1024 * 1024);
			assert!(large_lookup.all_lte(limit), "Must be possible to submit a large lookup");
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Anonymously schedule a task.
		#[pallet::call_index(0)]
		#[pallet::weight(<T as Config>::WeightInfo::schedule(T::MaxScheduledPerBlock::get()))]
		pub fn schedule(
			origin: OriginFor<T>,
			when: BlockNumberFor<T>,
			maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
			priority: schedule::Priority,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_schedule(
				DispatchTime::At(when),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				T::Preimages::bound(*call)?,
			)?;
			Ok(())
		}

		/// Cancel a scheduled task (named or anonymous), by providing the block it is scheduled for
		/// execution in, as well as the index of the task in that block's agenda.
		///
		/// In the case of a named task, it will remove it from the lookup table as well.
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config>::WeightInfo::cancel(T::MaxScheduledPerBlock::get()))]
		pub fn cancel(origin: OriginFor<T>, when: BlockNumberFor<T>, index: u32) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_cancel(Some(origin.caller().clone()), (when, index))?;
			Ok(())
		}

		/// Schedule a named task.
		#[pallet::call_index(2)]
		#[pallet::weight(<T as Config>::WeightInfo::schedule_named(T::MaxScheduledPerBlock::get()))]
		pub fn schedule_named(
			origin: OriginFor<T>,
			id: TaskName,
			when: BlockNumberFor<T>,
			maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
			priority: schedule::Priority,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_schedule_named(
				id,
				DispatchTime::At(when),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				T::Preimages::bound(*call)?,
			)?;
			Ok(())
		}

		/// Cancel a named scheduled task.
		#[pallet::call_index(3)]
		#[pallet::weight(<T as Config>::WeightInfo::cancel_named(T::MaxScheduledPerBlock::get()))]
		pub fn cancel_named(origin: OriginFor<T>, id: TaskName) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_cancel_named(Some(origin.caller().clone()), id)?;
			Ok(())
		}

		/// Anonymously schedule a task after a delay.
		#[pallet::call_index(4)]
		#[pallet::weight(<T as Config>::WeightInfo::schedule(T::MaxScheduledPerBlock::get()))]
		pub fn schedule_after(
			origin: OriginFor<T>,
			after: BlockNumberFor<T>,
			maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
			priority: schedule::Priority,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_schedule(
				DispatchTime::After(after),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				T::Preimages::bound(*call)?,
			)?;
			Ok(())
		}

		/// Schedule a named task after a delay.
		#[pallet::call_index(5)]
		#[pallet::weight(<T as Config>::WeightInfo::schedule_named(T::MaxScheduledPerBlock::get()))]
		pub fn schedule_named_after(
			origin: OriginFor<T>,
			id: TaskName,
			after: BlockNumberFor<T>,
			maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
			priority: schedule::Priority,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_schedule_named(
				id,
				DispatchTime::After(after),
				maybe_periodic,
				priority,
				origin.caller().clone(),
				T::Preimages::bound(*call)?,
			)?;
			Ok(())
		}

		/// Set a retry configuration for a task so that, in case its scheduled run fails, it will
		/// be retried after `period` blocks, for a total amount of `retries` retries or until it
		/// succeeds.
		///
		/// Tasks which need to be scheduled for a retry are still subject to weight metering and
		/// agenda space, same as a regular task. If a periodic task fails, it will be scheduled
		/// normally while the task is retrying.
		///
		/// Tasks scheduled as a result of a retry for a periodic task are unnamed, non-periodic
		/// clones of the original task. Their retry configuration will be derived from the
		/// original task's configuration, but will have a lower value for `remaining` than the
		/// original `total_retries`.
		///
		/// This call **cannot** be used to set a retry configuration for a named task.
		#[pallet::call_index(6)]
		#[pallet::weight(<T as Config>::WeightInfo::set_retry())]
		pub fn set_retry(
			origin: OriginFor<T>,
			task: TaskAddress<BlockNumberFor<T>>,
			retries: u8,
			period: BlockNumberFor<T>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			let (when, index) = task;
			let agenda = Agenda::<T>::get(when);
			let scheduled = agenda
				.get(index as usize)
				.and_then(Option::as_ref)
				.ok_or(Error::<T>::NotFound)?;
			Self::ensure_privilege(origin.caller(), &scheduled.origin)?;
			Retries::<T>::insert(
				(when, index),
				RetryConfig { total_retries: retries, remaining: retries, period },
			);
			Self::deposit_event(Event::RetrySet { task, id: None, period, retries });
			Ok(())
		}

		/// Set a retry configuration for a named task so that, in case its scheduled run fails, it
		/// will be retried after `period` blocks, for a total amount of `retries` retries or until
		/// it succeeds.
		///
		/// Tasks which need to be scheduled for a retry are still subject to weight metering and
		/// agenda space, same as a regular task. If a periodic task fails, it will be scheduled
		/// normally while the task is retrying.
		///
		/// Tasks scheduled as a result of a retry for a periodic task are unnamed, non-periodic
		/// clones of the original task. Their retry configuration will be derived from the
		/// original task's configuration, but will have a lower value for `remaining` than the
		/// original `total_retries`.
		///
		/// This is the only way to set a retry configuration for a named task.
		#[pallet::call_index(7)]
		#[pallet::weight(<T as Config>::WeightInfo::set_retry_named())]
		pub fn set_retry_named(
			origin: OriginFor<T>,
			id: TaskName,
			retries: u8,
			period: BlockNumberFor<T>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			let (when, agenda_index) = Lookup::<T>::get(&id).ok_or(Error::<T>::NotFound)?;
			let agenda = Agenda::<T>::get(when);
			let scheduled = agenda
				.get(agenda_index as usize)
				.and_then(Option::as_ref)
				.ok_or(Error::<T>::NotFound)?;
			Self::ensure_privilege(origin.caller(), &scheduled.origin)?;
			Retries::<T>::insert(
				(when, agenda_index),
				RetryConfig { total_retries: retries, remaining: retries, period },
			);
			Self::deposit_event(Event::RetrySet {
				task: (when, agenda_index),
				id: Some(id),
				period,
				retries,
			});
			Ok(())
		}

		/// Removes the retry configuration of a task.
		#[pallet::call_index(8)]
		#[pallet::weight(<T as Config>::WeightInfo::cancel_retry())]
		pub fn cancel_retry(
			origin: OriginFor<T>,
			task: TaskAddress<BlockNumberFor<T>>,
		) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			Self::do_cancel_retry(origin.caller(), task)?;
			Self::deposit_event(Event::RetryCancelled { task, id: None });
			Ok(())
		}

		/// Cancel the retry configuration of a named task.
		#[pallet::call_index(9)]
		#[pallet::weight(<T as Config>::WeightInfo::cancel_retry_named())]
		pub fn cancel_retry_named(origin: OriginFor<T>, id: TaskName) -> DispatchResult {
			T::ScheduleOrigin::ensure_origin(origin.clone())?;
			let origin = <T as Config>::RuntimeOrigin::from(origin);
			let task = Lookup::<T>::get(&id).ok_or(Error::<T>::NotFound)?;
			Self::do_cancel_retry(origin.caller(), task)?;
			Self::deposit_event(Event::RetryCancelled { task, id: Some(id) });
			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Migrate storage format from V1 to V4.
	///
	/// Returns the weight consumed by this migration.
	pub fn migrate_v1_to_v4() -> Weight {
		use migration::v1 as old;
		let mut weight = T::DbWeight::get().reads_writes(1, 1);

		// Delete all undecodable values.
		// `StorageMap::translate` is not enough since it just skips them and leaves the keys in.
		let keys = old::Agenda::<T>::iter_keys().collect::<Vec<_>>();
		for key in keys {
			weight.saturating_accrue(T::DbWeight::get().reads(1));
			if let Err(_) = old::Agenda::<T>::try_get(&key) {
				weight.saturating_accrue(T::DbWeight::get().writes(1));
				old::Agenda::<T>::remove(&key);
				log::warn!("Deleted undecodable agenda");
			}
		}

		Agenda::<T>::translate::<
			Vec<Option<ScheduledV1<<T as Config>::RuntimeCall, BlockNumberFor<T>>>>,
			_,
		>(|_, agenda| {
			Some(BoundedVec::truncate_from(
				agenda
					.into_iter()
					.map(|schedule| {
						weight.saturating_accrue(T::DbWeight::get().reads_writes(1, 1));

						schedule.and_then(|schedule| {
							if let Some(id) = schedule.maybe_id.as_ref() {
								let name = blake2_256(id);
								if let Some(item) = old::Lookup::<T>::take(id) {
									Lookup::<T>::insert(name, item);
								}
								weight.saturating_accrue(T::DbWeight::get().reads_writes(2, 2));
							}

							let call = T::Preimages::bound(schedule.call).ok()?;

							if call.lookup_needed() {
								weight.saturating_accrue(T::DbWeight::get().reads_writes(0, 1));
							}

							Some(Scheduled {
								maybe_id: schedule.maybe_id.map(|x| blake2_256(&x[..])),
								priority: schedule.priority,
								call,
								maybe_periodic: schedule.maybe_periodic,
								origin: system::RawOrigin::Root.into(),
								_phantom: Default::default(),
							})
						})
					})
					.collect::<Vec<_>>(),
			))
		});

		let _ = frame_support::storage::migration::clear_storage_prefix(
			Self::name().as_bytes(),
			b"StorageVersion",
			&[],
			None,
			None,
		);

		StorageVersion::new(4).put::<Self>();

		weight + T::DbWeight::get().writes(2)
	}

	/// Migrate storage format from V2 to V4.
	///
	/// Returns the weight consumed by this migration.
	pub fn migrate_v2_to_v4() -> Weight {
		use migration::v2 as old;
		let mut weight = T::DbWeight::get().reads_writes(1, 1);

		// Delete all undecodable values.
		// `StorageMap::translate` is not enough since it just skips them and leaves the keys in.
		let keys = old::Agenda::<T>::iter_keys().collect::<Vec<_>>();
		for key in keys {
			weight.saturating_accrue(T::DbWeight::get().reads(1));
			if let Err(_) = old::Agenda::<T>::try_get(&key) {
				weight.saturating_accrue(T::DbWeight::get().writes(1));
				old::Agenda::<T>::remove(&key);
				log::warn!("Deleted undecodable agenda");
			}
		}

		Agenda::<T>::translate::<Vec<Option<ScheduledV2Of<T>>>, _>(|_, agenda| {
			Some(BoundedVec::truncate_from(
				agenda
					.into_iter()
					.map(|schedule| {
						weight.saturating_accrue(T::DbWeight::get().reads_writes(1, 1));
						schedule.and_then(|schedule| {
							if let Some(id) = schedule.maybe_id.as_ref() {
								let name = blake2_256(id);
								if let Some(item) = old::Lookup::<T>::take(id) {
									Lookup::<T>::insert(name, item);
								}
								weight.saturating_accrue(T::DbWeight::get().reads_writes(2, 2));
							}

							let call = T::Preimages::bound(schedule.call).ok()?;
							if call.lookup_needed() {
								weight.saturating_accrue(T::DbWeight::get().reads_writes(0, 1));
							}

							Some(Scheduled {
								maybe_id: schedule.maybe_id.map(|x| blake2_256(&x[..])),
								priority: schedule.priority,
								call,
								maybe_periodic: schedule.maybe_periodic,
								origin: schedule.origin,
								_phantom: Default::default(),
							})
						})
					})
					.collect::<Vec<_>>(),
			))
		});

		let _ = frame_support::storage::migration::clear_storage_prefix(
			Self::name().as_bytes(),
			b"StorageVersion",
			&[],
			None,
			None,
		);

		StorageVersion::new(4).put::<Self>();

		weight + T::DbWeight::get().writes(2)
	}

	/// Migrate storage format from V3 to V4.
	///
	/// Returns the weight consumed by this migration.
	#[allow(deprecated)]
	pub fn migrate_v3_to_v4() -> Weight {
		use migration::v3 as old;
		let mut weight = T::DbWeight::get().reads_writes(2, 1);

		// Delete all undecodable values.
		// `StorageMap::translate` is not enough since it just skips them and leaves the keys in.
		let blocks = old::Agenda::<T>::iter_keys().collect::<Vec<_>>();
		for block in blocks {
			weight.saturating_accrue(T::DbWeight::get().reads(1));
			if let Err(_) = old::Agenda::<T>::try_get(&block) {
				weight.saturating_accrue(T::DbWeight::get().writes(1));
				old::Agenda::<T>::remove(&block);
				log::warn!("Deleted undecodable agenda of block: {:?}", block);
			}
		}

		Agenda::<T>::translate::<Vec<Option<ScheduledV3Of<T>>>, _>(|block, agenda| {
			log::info!("Migrating agenda of block: {:?}", &block);
			Some(BoundedVec::truncate_from(
				agenda
					.into_iter()
					.map(|schedule| {
						weight.saturating_accrue(T::DbWeight::get().reads_writes(1, 1));
						schedule
							.and_then(|schedule| {
								if let Some(id) = schedule.maybe_id.as_ref() {
									let name = blake2_256(id);
									if let Some(item) = old::Lookup::<T>::take(id) {
										Lookup::<T>::insert(name, item);
										log::info!("Migrated name for id: {:?}", id);
									} else {
										log::error!("No name in Lookup for id: {:?}", &id);
									}
									weight.saturating_accrue(T::DbWeight::get().reads_writes(2, 2));
								} else {
									log::info!("Schedule is unnamed");
								}

								let call = match schedule.call {
									MaybeHashed::Hash(h) => {
										let bounded = Bounded::from_legacy_hash(h);
										// Check that the call can be decoded in the new runtime.
										if let Err(err) = T::Preimages::peek::<
											<T as Config>::RuntimeCall,
										>(&bounded)
										{
											log::error!(
												"Dropping undecodable call {:?}: {:?}",
												&h,
												&err
											);
											return None;
										}
										weight.saturating_accrue(T::DbWeight::get().reads(1));
										log::info!("Migrated call by hash, hash: {:?}", h);
										bounded
									},
									MaybeHashed::Value(v) => {
										let call = T::Preimages::bound(v)
											.map_err(|e| {
												log::error!("Could not bound Call: {:?}", e)
											})
											.ok()?;
										if call.lookup_needed() {
											weight.saturating_accrue(
												T::DbWeight::get().reads_writes(0, 1),
											);
										}
										log::info!(
											"Migrated call by value, hash: {:?}",
											call.hash()
										);
										call
									},
								};

								Some(Scheduled {
									maybe_id: schedule.maybe_id.map(|x| blake2_256(&x[..])),
									priority: schedule.priority,
									call,
									maybe_periodic: schedule.maybe_periodic,
									origin: schedule.origin,
									_phantom: Default::default(),
								})
							})
							.or_else(|| {
								log::info!("Schedule in agenda for block {:?} is empty - nothing to do here.", &block);
								None
							})
					})
					.collect::<Vec<_>>(),
			))
		});

		let _ = frame_support::storage::migration::clear_storage_prefix(
			Self::name().as_bytes(),
			b"StorageVersion",
			&[],
			None,
			None,
		);

		StorageVersion::new(4).put::<Self>();

		weight + T::DbWeight::get().writes(2)
	}
}

impl<T: Config> Pallet<T> {
	/// Helper to migrate scheduler when the pallet origin type has changed.
	pub fn migrate_origin<OldOrigin: Into<T::PalletsOrigin> + codec::Decode>() {
		Agenda::<T>::translate::<
			Vec<
				Option<
					Scheduled<
						TaskName,
						BoundedCallOf<T>,
						BlockNumberFor<T>,
						OldOrigin,
						T::AccountId,
					>,
				>,
			>,
			_,
		>(|_, agenda| {
			Some(BoundedVec::truncate_from(
				agenda
					.into_iter()
					.map(|schedule| {
						schedule.map(|schedule| Scheduled {
							maybe_id: schedule.maybe_id,
							priority: schedule.priority,
							call: schedule.call,
							maybe_periodic: schedule.maybe_periodic,
							origin: schedule.origin.into(),
							_phantom: Default::default(),
						})
					})
					.collect::<Vec<_>>(),
			))
		});
	}

	fn resolve_time(
		when: DispatchTime<BlockNumberFor<T>>,
	) -> Result<BlockNumberFor<T>, DispatchError> {
		let now = T::BlockNumberProvider::current_block_number();
		let when = match when {
			DispatchTime::At(x) => x,
			// The current block has already completed it's scheduled tasks, so
			// Schedule the task at lest one block after this current block.
			DispatchTime::After(x) => now.saturating_add(x).saturating_add(One::one()),
		};

		if when <= now {
			return Err(Error::<T>::TargetBlockNumberInPast.into());
		}

		Ok(when)
	}

	fn place_task(
		when: BlockNumberFor<T>,
		what: ScheduledOf<T>,
	) -> Result<TaskAddress<BlockNumberFor<T>>, (DispatchError, ScheduledOf<T>)> {
		let maybe_name = what.maybe_id;
		let index = Self::push_to_agenda(when, what)?;
		let address = (when, index);
		if let Some(name) = maybe_name {
			Lookup::<T>::insert(name, address)
		}
		Self::deposit_event(Event::Scheduled { when: address.0, index: address.1 });
		Ok(address)
	}

	fn push_to_agenda(
		when: BlockNumberFor<T>,
		what: ScheduledOf<T>,
	) -> Result<u32, (DispatchError, ScheduledOf<T>)> {
		let mut agenda = Agenda::<T>::get(when);
		let index = if (agenda.len() as u32) < T::MaxScheduledPerBlock::get() {
			// will always succeed due to the above check.
			let _ = agenda.try_push(Some(what));
			agenda.len() as u32 - 1
		} else {
			if let Some(hole_index) = agenda.iter().position(|i| i.is_none()) {
				agenda[hole_index] = Some(what);
				hole_index as u32
			} else {
				return Err((DispatchError::Exhausted, what));
			}
		};
		Agenda::<T>::insert(when, agenda);
		Ok(index)
	}

	/// Remove trailing `None` items of an agenda at `when`. If all items are `None` remove the
	/// agenda record entirely.
	fn cleanup_agenda(when: BlockNumberFor<T>) {
		let mut agenda = Agenda::<T>::get(when);
		match agenda.iter().rposition(|i| i.is_some()) {
			// Note that `agenda.len() > i + 1` implies that the agenda ends on a sequence of at
			// least one `None` item(s).
			Some(i) if agenda.len() > i + 1 => {
				agenda.truncate(i + 1);
				Agenda::<T>::insert(when, agenda);
			},
			// This branch is taken if `agenda.len() <= i + 1 ==> agenda.len() == i + 1 <==>
			// agenda.len() - 1 == i` i.e. the agenda's last item is `Some`.
			Some(_) => {},
			// All items in the agenda are `None`.
			None => {
				Agenda::<T>::remove(when);
			},
		}
	}

	fn do_schedule(
		when: DispatchTime<BlockNumberFor<T>>,
		maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: BoundedCallOf<T>,
	) -> Result<TaskAddress<BlockNumberFor<T>>, DispatchError> {
		let when = Self::resolve_time(when)?;

		let lookup_hash = call.lookup_hash();

		// sanitize maybe_periodic
		let maybe_periodic = maybe_periodic
			.filter(|p| p.1 > 1 && !p.0.is_zero())
			// Remove one from the number of repetitions since we will schedule one now.
			.map(|(p, c)| (p, c - 1));
		let task = Scheduled {
			maybe_id: None,
			priority,
			call,
			maybe_periodic,
			origin,
			_phantom: PhantomData,
		};
		let res = Self::place_task(when, task).map_err(|x| x.0)?;

		if let Some(hash) = lookup_hash {
			// Request the call to be made available.
			T::Preimages::request(&hash);
		}

		Ok(res)
	}

	fn do_cancel(
		origin: Option<T::PalletsOrigin>,
		(when, index): TaskAddress<BlockNumberFor<T>>,
	) -> Result<(), DispatchError> {
		let scheduled = Agenda::<T>::try_mutate(when, |agenda| {
			agenda.get_mut(index as usize).map_or(
				Ok(None),
				|s| -> Result<Option<Scheduled<_, _, _, _, _>>, DispatchError> {
					if let (Some(ref o), Some(ref s)) = (origin, s.borrow()) {
						Self::ensure_privilege(o, &s.origin)?;
					};
					Ok(s.take())
				},
			)
		})?;
		if let Some(s) = scheduled {
			T::Preimages::drop(&s.call);
			if let Some(id) = s.maybe_id {
				Lookup::<T>::remove(id);
			}
			Retries::<T>::remove((when, index));
			Self::cleanup_agenda(when);
			Self::deposit_event(Event::Canceled { when, index });
			Ok(())
		} else {
			return Err(Error::<T>::NotFound.into());
		}
	}

	fn do_reschedule(
		(when, index): TaskAddress<BlockNumberFor<T>>,
		new_time: DispatchTime<BlockNumberFor<T>>,
	) -> Result<TaskAddress<BlockNumberFor<T>>, DispatchError> {
		let new_time = Self::resolve_time(new_time)?;

		if new_time == when {
			return Err(Error::<T>::RescheduleNoChange.into());
		}

		let task = Agenda::<T>::try_mutate(when, |agenda| {
			let task = agenda.get_mut(index as usize).ok_or(Error::<T>::NotFound)?;
			ensure!(!matches!(task, Some(Scheduled { maybe_id: Some(_), .. })), Error::<T>::Named);
			task.take().ok_or(Error::<T>::NotFound)
		})?;
		Self::cleanup_agenda(when);
		Self::deposit_event(Event::Canceled { when, index });

		Self::place_task(new_time, task).map_err(|x| x.0)
	}

	fn do_schedule_named(
		id: TaskName,
		when: DispatchTime<BlockNumberFor<T>>,
		maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: BoundedCallOf<T>,
	) -> Result<TaskAddress<BlockNumberFor<T>>, DispatchError> {
		// ensure id it is unique
		if Lookup::<T>::contains_key(&id) {
			return Err(Error::<T>::FailedToSchedule.into());
		}

		let when = Self::resolve_time(when)?;

		let lookup_hash = call.lookup_hash();

		// sanitize maybe_periodic
		let maybe_periodic = maybe_periodic
			.filter(|p| p.1 > 1 && !p.0.is_zero())
			// Remove one from the number of repetitions since we will schedule one now.
			.map(|(p, c)| (p, c - 1));

		let task = Scheduled {
			maybe_id: Some(id),
			priority,
			call,
			maybe_periodic,
			origin,
			_phantom: Default::default(),
		};
		let res = Self::place_task(when, task).map_err(|x| x.0)?;

		if let Some(hash) = lookup_hash {
			// Request the call to be made available.
			T::Preimages::request(&hash);
		}

		Ok(res)
	}

	fn do_cancel_named(origin: Option<T::PalletsOrigin>, id: TaskName) -> DispatchResult {
		Lookup::<T>::try_mutate_exists(id, |lookup| -> DispatchResult {
			if let Some((when, index)) = lookup.take() {
				let i = index as usize;
				Agenda::<T>::try_mutate(when, |agenda| -> DispatchResult {
					if let Some(s) = agenda.get_mut(i) {
						if let (Some(ref o), Some(ref s)) = (origin, s.borrow()) {
							Self::ensure_privilege(o, &s.origin)?;
							Retries::<T>::remove((when, index));
							T::Preimages::drop(&s.call);
						}
						*s = None;
					}
					Ok(())
				})?;
				Self::cleanup_agenda(when);
				Self::deposit_event(Event::Canceled { when, index });
				Ok(())
			} else {
				return Err(Error::<T>::NotFound.into());
			}
		})
	}

	fn do_reschedule_named(
		id: TaskName,
		new_time: DispatchTime<BlockNumberFor<T>>,
	) -> Result<TaskAddress<BlockNumberFor<T>>, DispatchError> {
		let new_time = Self::resolve_time(new_time)?;

		let lookup = Lookup::<T>::get(id);
		let (when, index) = lookup.ok_or(Error::<T>::NotFound)?;

		if new_time == when {
			return Err(Error::<T>::RescheduleNoChange.into());
		}

		let task = Agenda::<T>::try_mutate(when, |agenda| {
			let task = agenda.get_mut(index as usize).ok_or(Error::<T>::NotFound)?;
			task.take().ok_or(Error::<T>::NotFound)
		})?;
		Self::cleanup_agenda(when);
		Self::deposit_event(Event::Canceled { when, index });
		Self::place_task(new_time, task).map_err(|x| x.0)
	}

	fn do_cancel_retry(
		origin: &T::PalletsOrigin,
		(when, index): TaskAddress<BlockNumberFor<T>>,
	) -> Result<(), DispatchError> {
		let agenda = Agenda::<T>::get(when);
		let scheduled = agenda
			.get(index as usize)
			.and_then(Option::as_ref)
			.ok_or(Error::<T>::NotFound)?;
		Self::ensure_privilege(origin, &scheduled.origin)?;
		Retries::<T>::remove((when, index));
		Ok(())
	}
}

enum ServiceTaskError {
	/// Could not be executed due to missing preimage.
	Unavailable,
	/// Could not be executed due to weight limitations.
	Overweight,
}
use ServiceTaskError::*;

impl<T: Config> Pallet<T> {
	/// Service up to `max` agendas queue starting from earliest incompletely executed agenda.
	fn service_agendas(weight: &mut WeightMeter, now: BlockNumberFor<T>, max: u32) {
		if weight.try_consume(T::WeightInfo::service_agendas_base()).is_err() {
			return;
		}

		let mut incomplete_since = now + One::one();
		let mut when = IncompleteSince::<T>::take().unwrap_or(now);
		let mut is_first = true; // first task from the first agenda.

		let max_items = T::MaxScheduledPerBlock::get();
		let mut count_down = max;
		let service_agenda_base_weight = T::WeightInfo::service_agenda_base(max_items);
		while count_down > 0 && when <= now && weight.can_consume(service_agenda_base_weight) {
			if !Self::service_agenda(weight, is_first, now, when, u32::MAX) {
				incomplete_since = incomplete_since.min(when);
			}
			is_first = false;
			when.saturating_inc();
			count_down.saturating_dec();
		}
		incomplete_since = incomplete_since.min(when);
		if incomplete_since <= now {
			Self::deposit_event(Event::AgendaIncomplete { when: incomplete_since });
			IncompleteSince::<T>::put(incomplete_since);
		} else {
			// The next scheduler iteration should typically start from `now + 1` (`next_iter_now`).
			// However, if the [`Config::BlockNumberProvider`] is not a local block number provider,
			// then `next_iter_now` could be `now + n` where `n > 1`. In this case, we want to start
			// from `now + 1` to ensure we don't miss any agendas.
			IncompleteSince::<T>::put(now + One::one());
		}
	}

	/// Returns `true` if the agenda was fully completed, `false` if it should be revisited at a
	/// later block.
	fn service_agenda(
		weight: &mut WeightMeter,
		mut is_first: bool,
		now: BlockNumberFor<T>,
		when: BlockNumberFor<T>,
		max: u32,
	) -> bool {
		let mut agenda = Agenda::<T>::get(when);
		let mut ordered = agenda
			.iter()
			.enumerate()
			.filter_map(|(index, maybe_item)| {
				maybe_item.as_ref().map(|item| (index as u32, item.priority))
			})
			.collect::<Vec<_>>();
		ordered.sort_by_key(|k| k.1);
		let within_limit = weight
			.try_consume(T::WeightInfo::service_agenda_base(ordered.len() as u32))
			.is_ok();
		debug_assert!(within_limit, "weight limit should have been checked in advance");

		// Items which we know can be executed and have postponed for execution in a later block.
		let mut postponed = (ordered.len() as u32).saturating_sub(max);
		// Items which we don't know can ever be executed.
		let mut dropped = 0;

		for (agenda_index, _) in ordered.into_iter().take(max as usize) {
			let Some(task) = agenda[agenda_index as usize].take() else { continue };
			let base_weight = T::WeightInfo::service_task(
				task.call.lookup_len().map(|x| x as usize),
				task.maybe_id.is_some(),
				task.maybe_periodic.is_some(),
			);
			if !weight.can_consume(base_weight) {
				postponed += 1;
				agenda[agenda_index as usize] = Some(task);
				break;
			}
			let result = Self::service_task(weight, now, when, agenda_index, is_first, task);
			agenda[agenda_index as usize] = match result {
				Err((Unavailable, slot)) => {
					dropped += 1;
					slot
				},
				Err((Overweight, slot)) => {
					postponed += 1;
					slot
				},
				Ok(()) => {
					is_first = false;
					None
				},
			};
		}
		if postponed > 0 || dropped > 0 {
			Agenda::<T>::insert(when, agenda);
		} else {
			Agenda::<T>::remove(when);
		}

		postponed == 0
	}

	/// Service (i.e. execute) the given task, being careful not to overflow the `weight` counter.
	///
	/// This involves:
	/// - removing and potentially replacing the `Lookup` entry for the task.
	/// - realizing the task's call which can include a preimage lookup.
	/// - Rescheduling the task for execution in a later agenda if periodic.
	fn service_task(
		weight: &mut WeightMeter,
		now: BlockNumberFor<T>,
		when: BlockNumberFor<T>,
		agenda_index: u32,
		is_first: bool,
		mut task: ScheduledOf<T>,
	) -> Result<(), (ServiceTaskError, Option<ScheduledOf<T>>)> {
		if let Some(ref id) = task.maybe_id {
			Lookup::<T>::remove(id);
		}

		let (call, lookup_len) = match T::Preimages::peek(&task.call) {
			Ok(c) => c,
			Err(_) => {
				Self::deposit_event(Event::CallUnavailable {
					task: (when, agenda_index),
					id: task.maybe_id,
				});

				// It was not available when we needed it, so we don't need to have requested it
				// anymore.
				T::Preimages::drop(&task.call);

				// We don't know why `peek` failed, thus we most account here for the "full weight".
				let _ = weight.try_consume(T::WeightInfo::service_task(
					task.call.lookup_len().map(|x| x as usize),
					task.maybe_id.is_some(),
					task.maybe_periodic.is_some(),
				));

				return Err((Unavailable, Some(task)));
			},
		};

		let _ = weight.try_consume(T::WeightInfo::service_task(
			lookup_len.map(|x| x as usize),
			task.maybe_id.is_some(),
			task.maybe_periodic.is_some(),
		));

		match Self::execute_dispatch(weight, task.origin.clone(), call) {
			Err(()) if is_first => {
				T::Preimages::drop(&task.call);
				Self::deposit_event(Event::PermanentlyOverweight {
					task: (when, agenda_index),
					id: task.maybe_id,
				});
				Err((Unavailable, Some(task)))
			},
			Err(()) => Err((Overweight, Some(task))),
			Ok(result) => {
				let failed = result.is_err();
				let maybe_retry_config = Retries::<T>::take((when, agenda_index));
				Self::deposit_event(Event::Dispatched {
					task: (when, agenda_index),
					id: task.maybe_id,
					result,
				});

				match maybe_retry_config {
					Some(retry_config) if failed => {
						Self::schedule_retry(weight, now, when, agenda_index, &task, retry_config);
					},
					_ => {},
				}

				if let &Some((period, count)) = &task.maybe_periodic {
					if count > 1 {
						task.maybe_periodic = Some((period, count - 1));
					} else {
						task.maybe_periodic = None;
					}
					let wake = now.saturating_add(period);
					match Self::place_task(wake, task) {
						Ok(new_address) => {
							if let Some(retry_config) = maybe_retry_config {
								Retries::<T>::insert(new_address, retry_config);
							}
						},
						Err((_, task)) => {
							// TODO: Leave task in storage somewhere for it to be rescheduled
							// manually.
							T::Preimages::drop(&task.call);
							Self::deposit_event(Event::PeriodicFailed {
								task: (when, agenda_index),
								id: task.maybe_id,
							});
						},
					}
				} else {
					T::Preimages::drop(&task.call);
				}
				Ok(())
			},
		}
	}

	/// Make a dispatch to the given `call` from the given `origin`, ensuring that the `weight`
	/// counter does not exceed its limit and that it is counted accurately (e.g. accounted using
	/// post info if available).
	///
	/// NOTE: Only the weight for this function will be counted (origin lookup, dispatch and the
	/// call itself).
	///
	/// Returns an error if the call is overweight.
	fn execute_dispatch(
		weight: &mut WeightMeter,
		origin: T::PalletsOrigin,
		call: <T as Config>::RuntimeCall,
	) -> Result<DispatchResult, ()> {
		let base_weight = match origin.as_system_ref() {
			Some(&RawOrigin::Signed(_)) => T::WeightInfo::execute_dispatch_signed(),
			_ => T::WeightInfo::execute_dispatch_unsigned(),
		};
		let call_weight = call.get_dispatch_info().call_weight;
		// We only allow a scheduled call if it cannot push the weight past the limit.
		let max_weight = base_weight.saturating_add(call_weight);

		if !weight.can_consume(max_weight) {
			return Err(());
		}

		let dispatch_origin = origin.into();
		let (maybe_actual_call_weight, result) = match call.dispatch(dispatch_origin) {
			Ok(post_info) => (post_info.actual_weight, Ok(())),
			Err(error_and_info) => {
				(error_and_info.post_info.actual_weight, Err(error_and_info.error))
			},
		};
		let call_weight = maybe_actual_call_weight.unwrap_or(call_weight);
		let _ = weight.try_consume(base_weight);
		let _ = weight.try_consume(call_weight);
		Ok(result)
	}

	/// Check if a task has a retry configuration in place and, if so, try to reschedule it.
	///
	/// Possible causes for failure to schedule a retry for a task:
	/// - there wasn't enough weight to run the task reschedule logic
	/// - there was no retry configuration in place
	/// - there were no more retry attempts left
	/// - the agenda was full.
	fn schedule_retry(
		weight: &mut WeightMeter,
		now: BlockNumberFor<T>,
		when: BlockNumberFor<T>,
		agenda_index: u32,
		task: &ScheduledOf<T>,
		retry_config: RetryConfig<BlockNumberFor<T>>,
	) {
		if weight
			.try_consume(T::WeightInfo::schedule_retry(T::MaxScheduledPerBlock::get()))
			.is_err()
		{
			Self::deposit_event(Event::RetryFailed {
				task: (when, agenda_index),
				id: task.maybe_id,
			});
			return;
		}

		let RetryConfig { total_retries, mut remaining, period } = retry_config;
		remaining = match remaining.checked_sub(1) {
			Some(n) => n,
			None => return,
		};
		let wake = now.saturating_add(period);
		match Self::place_task(wake, task.as_retry()) {
			Ok(address) => {
				// Reinsert the retry config to the new address of the task after it was
				// placed.
				Retries::<T>::insert(address, RetryConfig { total_retries, remaining, period });
			},
			Err((_, task)) => {
				// TODO: Leave task in storage somewhere for it to be
				// rescheduled manually.
				T::Preimages::drop(&task.call);
				Self::deposit_event(Event::RetryFailed {
					task: (when, agenda_index),
					id: task.maybe_id,
				});
			},
		}
	}

	/// Ensure that `left` has at least the same level of privilege or higher than `right`.
	///
	/// Returns an error if `left` has a lower level of privilege or the two cannot be compared.
	fn ensure_privilege(
		left: &<T as Config>::PalletsOrigin,
		right: &<T as Config>::PalletsOrigin,
	) -> Result<(), DispatchError> {
		if matches!(T::OriginPrivilegeCmp::cmp_privilege(left, right), Some(Ordering::Less) | None)
		{
			return Err(BadOrigin.into());
		}
		Ok(())
	}
}

impl<T: Config> schedule::v3::Anon<BlockNumberFor<T>, <T as Config>::RuntimeCall, T::PalletsOrigin>
	for Pallet<T>
{
	type Address = TaskAddress<BlockNumberFor<T>>;
	type Hasher = T::Hashing;

	fn schedule(
		when: DispatchTime<BlockNumberFor<T>>,
		maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: BoundedCallOf<T>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_schedule(when, maybe_periodic, priority, origin, call)
	}

	fn cancel((when, index): Self::Address) -> Result<(), DispatchError> {
		Self::do_cancel(None, (when, index)).map_err(map_err_to_v3_err::<T>)
	}

	fn reschedule(
		address: Self::Address,
		when: DispatchTime<BlockNumberFor<T>>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_reschedule(address, when).map_err(map_err_to_v3_err::<T>)
	}

	fn next_dispatch_time(
		(when, index): Self::Address,
	) -> Result<BlockNumberFor<T>, DispatchError> {
		Agenda::<T>::get(when)
			.get(index as usize)
			.ok_or(DispatchError::Unavailable)
			.map(|_| when)
	}
}

use schedule::v3::TaskName;

impl<T: Config> schedule::v3::Named<BlockNumberFor<T>, <T as Config>::RuntimeCall, T::PalletsOrigin>
	for Pallet<T>
{
	type Address = TaskAddress<BlockNumberFor<T>>;
	type Hasher = T::Hashing;

	fn schedule_named(
		id: TaskName,
		when: DispatchTime<BlockNumberFor<T>>,
		maybe_periodic: Option<schedule::Period<BlockNumberFor<T>>>,
		priority: schedule::Priority,
		origin: T::PalletsOrigin,
		call: BoundedCallOf<T>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_schedule_named(id, when, maybe_periodic, priority, origin, call)
	}

	fn cancel_named(id: TaskName) -> Result<(), DispatchError> {
		Self::do_cancel_named(None, id).map_err(map_err_to_v3_err::<T>)
	}

	fn reschedule_named(
		id: TaskName,
		when: DispatchTime<BlockNumberFor<T>>,
	) -> Result<Self::Address, DispatchError> {
		Self::do_reschedule_named(id, when).map_err(map_err_to_v3_err::<T>)
	}

	fn next_dispatch_time(id: TaskName) -> Result<BlockNumberFor<T>, DispatchError> {
		Lookup::<T>::get(id)
			.and_then(|(when, index)| Agenda::<T>::get(when).get(index as usize).map(|_| when))
			.ok_or(DispatchError::Unavailable)
	}
}

/// Maps a pallet error to an `schedule::v3` error.
fn map_err_to_v3_err<T: Config>(err: DispatchError) -> DispatchError {
	if err == DispatchError::from(Error::<T>::NotFound) {
		DispatchError::Unavailable
	} else {
		err
	}
}
