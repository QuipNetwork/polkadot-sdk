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

#![cfg_attr(not(feature = "std"), no_std)]
#![doc = include_str!("../README.md")]

pub use pallet::*;

mod adapt_price;
mod benchmarking;
mod coretime_interface;
mod dispatchable_impls;

#[cfg(test)]
mod mock;
mod nonfungible_impl;
#[cfg(test)]
mod test_fungibles;
#[cfg(test)]
mod tests;
mod tick_impls;
mod types;
mod utility_impls;

pub mod migration;
pub mod runtime_api;

pub mod weights;
pub use weights::WeightInfo;

pub use adapt_price::*;
pub use coretime_interface::*;
pub use fp_coretime::{
	market, CoreIndex, CoreMask, PartsOf57600, PotentialRenewalId, RegionId, TaskId, Timeslice,
	CORE_MASK_BITS,
};
pub use types::*;

extern crate alloc;

/// The log target for this pallet.
const LOG_TARGET: &str = "runtime::broker";

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use alloc::vec::Vec;
	use frame_support::{
		pallet_prelude::{DispatchResult, DispatchResultWithPostInfo, *},
		traits::{
			fungible::{Balanced, Credit, Mutate},
			BuildGenesisConfig, EnsureOrigin, OnUnbalanced,
		},
		PalletId,
	};
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::{Convert, ConvertBack, MaybeConvert};

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(5);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Weight information for all calls of this pallet.
		type WeightInfo: WeightInfo;

		/// Currency used to pay for Coretime.
		type Currency: Mutate<Self::AccountId> + Balanced<Self::AccountId>;

		/// The origin test needed for administrating this pallet.
		type AdminOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// What to do with any revenues collected from the sale of Coretime.
		type OnRevenue: OnUnbalanced<Credit<Self::AccountId, Self::Currency>>;

		/// Relay chain's Coretime API used to interact with and instruct the low-level scheduling
		/// system.
		type Coretime: CoretimeInterface;

		/// The algorithm to determine the next price on the basis of market performance.
		type PriceAdapter: AdaptPrice<BalanceOf<Self>>;

		/// Reversible conversion from local balance to Relay-chain balance. This will typically be
		/// the `Identity`, but provided just in case the chains use different representations.
		type ConvertBalance: Convert<BalanceOf<Self>, RelayBalanceOf<Self>>
			+ ConvertBack<BalanceOf<Self>, RelayBalanceOf<Self>>;

		/// Type used for getting the associated account of a task. This account is controlled by
		/// the task itself.
		type SovereignAccountOf: MaybeConvert<TaskId, Self::AccountId>;

		/// Identifier from which the internal Pot is generated.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Number of Relay-chain blocks per timeslice.
		#[pallet::constant]
		type TimeslicePeriod: Get<RelayBlockNumberOf<Self>>;

		/// Maximum number of legacy leases.
		#[pallet::constant]
		type MaxLeasedCores: Get<u32>;

		/// Maximum number of system cores.
		#[pallet::constant]
		type MaxReservedCores: Get<u32>;

		/// Given that we are performing all auto-renewals in a single block, it has to be limited.
		#[pallet::constant]
		type MaxAutoRenewals: Get<u32>;

		/// The smallest amount of credits a user can purchase.
		///
		/// Needed to prevent spam attacks.
		#[pallet::constant]
		type MinimumCreditPurchase: Get<BalanceOf<Self>>;
	}

	/// The current configuration of this pallet.
	#[pallet::storage]
	pub type Configuration<T> = StorageValue<_, ConfigRecordOf<T>, OptionQuery>;

	/// The Polkadot Core reservations (generally tasked with the maintenance of System Chains).
	#[pallet::storage]
	pub type Reservations<T> = StorageValue<_, ReservationsRecordOf<T>, ValueQuery>;

	/// Force reservations that need to be inserted into the workplan at the next sale rotation.
	///
	/// They are automatically freed at the next sale rotation.
	#[pallet::storage]
	pub type ForceReservations<T> = StorageValue<_, ReservationsRecordOf<T>, ValueQuery>;

	/// The Polkadot Core legacy leases.
	#[pallet::storage]
	pub type Leases<T> = StorageValue<_, LeasesRecordOf<T>, ValueQuery>;

	/// The current status of miscellaneous subsystems of this pallet.
	#[pallet::storage]
	pub type Status<T> = StorageValue<_, StatusRecord, OptionQuery>;

	/// The details of the current sale, including its properties and status.
	#[pallet::storage]
	pub type SaleInfo<T> = StorageValue<_, SaleInfoRecordOf<T>, OptionQuery>;

	/// Records of potential renewals.
	///
	/// Renewals will only actually be allowed if `CompletionStatus` is actually `Complete`.
	#[pallet::storage]
	pub type PotentialRenewals<T> =
		StorageMap<_, Twox64Concat, PotentialRenewalId, PotentialRenewalRecordOf<T>, OptionQuery>;

	/// The current (unassigned or provisionally assigend) Regions.
	#[pallet::storage]
	pub type Regions<T> = StorageMap<_, Blake2_128Concat, RegionId, RegionRecordOf<T>, OptionQuery>;

	/// The work we plan on having each core do at a particular time in the future.
	#[pallet::storage]
	pub type Workplan<T> =
		StorageMap<_, Twox64Concat, (Timeslice, CoreIndex), Schedule, OptionQuery>;

	/// The current workload of each core. This gets updated with workplan as timeslices pass.
	#[pallet::storage]
	pub type Workload<T> = StorageMap<_, Twox64Concat, CoreIndex, Schedule, ValueQuery>;

	/// Record of a single contribution to the Instantaneous Coretime Pool.
	#[pallet::storage]
	pub type InstaPoolContribution<T> =
		StorageMap<_, Blake2_128Concat, RegionId, ContributionRecordOf<T>, OptionQuery>;

	/// Record of Coretime entering or leaving the Instantaneous Coretime Pool.
	#[pallet::storage]
	pub type InstaPoolIo<T> = StorageMap<_, Blake2_128Concat, Timeslice, PoolIoRecord, ValueQuery>;

	/// Total InstaPool rewards for each Timeslice and the number of core parts which contributed.
	#[pallet::storage]
	pub type InstaPoolHistory<T> =
		StorageMap<_, Blake2_128Concat, Timeslice, InstaPoolHistoryRecordOf<T>>;

	/// Received core count change from the relay chain.
	#[pallet::storage]
	pub type CoreCountInbox<T> = StorageValue<_, CoreIndex, OptionQuery>;

	/// Keeping track of cores which have auto-renewal enabled.
	///
	/// Sorted by `CoreIndex` to make the removal of cores from auto-renewal more efficient.
	#[pallet::storage]
	pub type AutoRenewals<T: Config> =
		StorageValue<_, BoundedVec<AutoRenewalRecord, T::MaxAutoRenewals>, ValueQuery>;

	/// Received revenue info from the relay chain.
	#[pallet::storage]
	pub type RevenueInbox<T> = StorageValue<_, OnDemandRevenueRecordOf<T>, OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A Region of Bulk Coretime has been purchased.
		Purchased {
			/// The identity of the purchaser.
			who: T::AccountId,
			/// The identity of the Region.
			region_id: RegionId,
			/// The price paid for this Region.
			price: BalanceOf<T>,
			/// The duration of the Region.
			duration: Timeslice,
		},
		/// The workload of a core has become renewable.
		Renewable {
			/// The core whose workload can be renewed.
			core: CoreIndex,
			/// The price at which the workload can be renewed.
			price: BalanceOf<T>,
			/// The time at which the workload would recommence of this renewal. The call to renew
			/// cannot happen before the beginning of the interlude prior to the sale for regions
			/// which begin at this time.
			begin: Timeslice,
			/// The actual workload which can be renewed.
			workload: Schedule,
		},
		/// A workload has been renewed.
		Renewed {
			/// The identity of the renewer.
			who: T::AccountId,
			/// The price paid for this renewal.
			price: BalanceOf<T>,
			/// The index of the core on which the `workload` was previously scheduled.
			old_core: CoreIndex,
			/// The index of the core on which the renewed `workload` has been scheduled.
			core: CoreIndex,
			/// The time at which the `workload` will begin on the `core`.
			begin: Timeslice,
			/// The number of timeslices for which this `workload` is newly scheduled.
			duration: Timeslice,
			/// The workload which was renewed.
			workload: Schedule,
		},
		/// Ownership of a Region has been transferred.
		Transferred {
			/// The Region which has been transferred.
			region_id: RegionId,
			/// The duration of the Region.
			duration: Timeslice,
			/// The old owner of the Region.
			old_owner: Option<T::AccountId>,
			/// The new owner of the Region.
			owner: Option<T::AccountId>,
		},
		/// A Region has been split into two non-overlapping Regions.
		Partitioned {
			/// The Region which was split.
			old_region_id: RegionId,
			/// The new Regions into which it became.
			new_region_ids: (RegionId, RegionId),
		},
		/// A Region has been converted into two overlapping Regions each of lesser regularity.
		Interlaced {
			/// The Region which was interlaced.
			old_region_id: RegionId,
			/// The new Regions into which it became.
			new_region_ids: (RegionId, RegionId),
		},
		/// A Region has been assigned to a particular task.
		Assigned {
			/// The Region which was assigned.
			region_id: RegionId,
			/// The duration of the assignment.
			duration: Timeslice,
			/// The task to which the Region was assigned.
			task: TaskId,
		},
		/// An assignment has been removed from the workplan.
		AssignmentRemoved {
			/// The Region which was removed from the workplan.
			region_id: RegionId,
		},
		/// A Region has been added to the Instantaneous Coretime Pool.
		Pooled {
			/// The Region which was added to the Instantaneous Coretime Pool.
			region_id: RegionId,
			/// The duration of the Region.
			duration: Timeslice,
		},
		/// A new number of cores has been requested.
		CoreCountRequested {
			/// The number of cores requested.
			core_count: CoreIndex,
		},
		/// The number of cores available for scheduling has changed.
		CoreCountChanged {
			/// The new number of cores available for scheduling.
			core_count: CoreIndex,
		},
		/// There is a new reservation for a workload.
		ReservationMade {
			/// The index of the reservation.
			index: u32,
			/// The workload of the reservation.
			workload: Schedule,
		},
		/// A reservation for a workload has been cancelled.
		ReservationCancelled {
			/// The index of the reservation which was cancelled.
			index: u32,
			/// The workload of the now cancelled reservation.
			workload: Schedule,
		},
		/// A new sale has been initialized.
		SaleInitialized {
			/// The relay block number at which the sale will/did start.
			sale_start: RelayBlockNumberOf<T>,
			/// The length in relay chain blocks of the Leadin Period (where the price is
			/// decreasing).
			leadin_length: RelayBlockNumberOf<T>,
			/// The price of Bulk Coretime at the beginning of the Leadin Period.
			start_price: BalanceOf<T>,
			/// The price of Bulk Coretime after the Leadin Period.
			end_price: BalanceOf<T>,
			/// The first timeslice of the Regions which are being sold in this sale.
			region_begin: Timeslice,
			/// The timeslice on which the Regions which are being sold in the sale terminate.
			/// (i.e. One after the last timeslice which the Regions control.)
			region_end: Timeslice,
			/// The number of cores we want to sell, ideally.
			ideal_cores_sold: CoreIndex,
			/// Number of cores which are/have been offered for sale.
			cores_offered: CoreIndex,
			/// Sequential identifier for the current sale period.
			sale_index: SaleIndex,
		},
		/// A new lease has been created.
		Leased {
			/// The task to which a core will be assigned.
			task: TaskId,
			/// The timeslice contained in the sale period after which this lease will
			/// self-terminate (and therefore the earliest timeslice at which the lease may no
			/// longer apply).
			until: Timeslice,
		},
		/// A lease has been removed.
		LeaseRemoved {
			/// The task to which a core was assigned.
			task: TaskId,
		},
		/// A lease is about to end.
		LeaseEnding {
			/// The task to which a core was assigned.
			task: TaskId,
			/// The timeslice at which the task will no longer be scheduled.
			when: Timeslice,
		},
		/// The sale rotation has been started and a new sale is imminent.
		SalesStarted {
			/// The nominal price of an Region of Bulk Coretime.
			price: BalanceOf<T>,
			/// The maximum number of cores which this pallet will attempt to assign.
			core_count: CoreIndex,
		},
		/// The act of claiming revenue has begun.
		RevenueClaimBegun {
			/// The region to be claimed for.
			region: RegionId,
			/// The maximum number of timeslices which should be searched for claimed.
			max_timeslices: Timeslice,
		},
		/// A particular timeslice has a non-zero claim.
		RevenueClaimItem {
			/// The timeslice whose claim is being processed.
			when: Timeslice,
			/// The amount which was claimed at this timeslice.
			amount: BalanceOf<T>,
		},
		/// A revenue claim has (possibly only in part) been paid.
		RevenueClaimPaid {
			/// The account to whom revenue has been paid.
			who: T::AccountId,
			/// The total amount of revenue claimed and paid.
			amount: BalanceOf<T>,
			/// The next region which should be claimed for the continuation of this contribution.
			next: Option<RegionId>,
		},
		/// Some Instantaneous Coretime Pool credit has been purchased.
		CreditPurchased {
			/// The account which purchased the credit.
			who: T::AccountId,
			/// The Relay-chain account to which the credit will be made.
			beneficiary: RelayAccountIdOf<T>,
			/// The amount of credit purchased.
			amount: BalanceOf<T>,
		},
		/// A Region has been dropped due to being out of date.
		RegionDropped {
			/// The Region which no longer exists.
			region_id: RegionId,
			/// The duration of the Region.
			duration: Timeslice,
		},
		/// Some historical Instantaneous Core Pool contribution record has been dropped.
		ContributionDropped {
			/// The Region whose contribution is no longer exists.
			region_id: RegionId,
		},
		/// A region has been force-removed from the pool. This is usually due to a provisionally
		/// pooled region being redeployed.
		RegionUnpooled {
			/// The Region which has been force-removed from the pool.
			region_id: RegionId,
			/// The timeslice at which the region was force-removed.
			when: Timeslice,
		},
		/// Some historical Instantaneous Core Pool payment record has been initialized.
		HistoryInitialized {
			/// The timeslice whose history has been initialized.
			when: Timeslice,
			/// The amount of privately contributed Coretime to the Instantaneous Coretime Pool.
			private_pool_size: CoreMaskBitCount,
			/// The amount of Coretime contributed to the Instantaneous Coretime Pool by the
			/// Polkadot System.
			system_pool_size: CoreMaskBitCount,
		},
		/// Some historical Instantaneous Core Pool payment record has been dropped.
		HistoryDropped {
			/// The timeslice whose history is no longer available.
			when: Timeslice,
			/// The amount of revenue the system has taken.
			revenue: BalanceOf<T>,
		},
		/// Some historical Instantaneous Core Pool payment record has been ignored because the
		/// timeslice was already known. Governance may need to intervene.
		HistoryIgnored {
			/// The timeslice whose history is was ignored.
			when: Timeslice,
			/// The amount of revenue which was ignored.
			revenue: BalanceOf<T>,
		},
		/// Some historical Instantaneous Core Pool Revenue is ready for payout claims.
		ClaimsReady {
			/// The timeslice whose history is available.
			when: Timeslice,
			/// The amount of revenue the Polkadot System has already taken.
			system_payout: BalanceOf<T>,
			/// The total amount of revenue remaining to be claimed.
			private_payout: BalanceOf<T>,
		},
		/// A Core has been assigned to one or more tasks and/or the Pool on the Relay-chain.
		CoreAssigned {
			/// The index of the Core which has been assigned.
			core: CoreIndex,
			/// The Relay-chain block at which this assignment should take effect.
			when: RelayBlockNumberOf<T>,
			/// The workload to be done on the Core.
			assignment: Vec<(CoreAssignment, PartsOf57600)>,
		},
		/// Some historical Instantaneous Core Pool payment record has been dropped.
		PotentialRenewalDropped {
			/// The timeslice whose renewal is no longer available.
			when: Timeslice,
			/// The core whose workload is no longer available to be renewed for `when`.
			core: CoreIndex,
		},
		AutoRenewalEnabled {
			/// The core for which the renewal was enabled.
			core: CoreIndex,
			/// The task for which the renewal was enabled.
			task: TaskId,
		},
		AutoRenewalDisabled {
			/// The core for which the renewal was disabled.
			core: CoreIndex,
			/// The task for which the renewal was disabled.
			task: TaskId,
		},
		/// Failed to auto-renew a core, likely due to the payer account not being sufficiently
		/// funded or the workload on the core no longer belonging to the paying task.
		AutoRenewalFailed {
			/// The core for which the renewal failed.
			core: CoreIndex,
			/// The account which was supposed to pay for renewal.
			///
			/// If `None` it indicates that we failed to get the sovereign account of a task.
			payer: Option<T::AccountId>,
		},
		/// The auto-renewal limit has been reached upon renewing cores.
		///
		/// This should never happen, given that enable_auto_renew checks for this before enabling
		/// auto-renewal.
		AutoRenewalLimitReached,
		/// Failed to assign a force reservation due to no free cores available.
		ForceReservationFailed {
			/// The schedule that could not be assigned.
			schedule: Schedule,
		},
		/// Potential renewal was forcefully removed.
		PotentialRenewalRemoved {
			/// The core associated with the potential renewal that was removed.
			core: CoreIndex,
			/// The timeslice associated with the potential renewal that was removed.
			timeslice: Timeslice,
		},
	}

	#[pallet::error]
	#[derive(PartialEq)]
	pub enum Error<T> {
		/// The given region identity is not known.
		UnknownRegion,
		/// The owner of the region is not the origin.
		NotOwner,
		/// The pivot point of the partition at or after the end of the region.
		PivotTooLate,
		/// The pivot point of the partition at the beginning of the region.
		PivotTooEarly,
		/// The pivot mask for the interlacing is not contained within the region's interlace mask.
		ExteriorPivot,
		/// The pivot mask for the interlacing is void (and therefore unschedulable).
		VoidPivot,
		/// The pivot mask for the interlacing is complete (and therefore not a strict subset).
		CompletePivot,
		/// The workplan of the pallet's state is invalid. This indicates a state corruption.
		CorruptWorkplan,
		/// There is no sale happening currently.
		NoSales,
		/// The price limit is exceeded.
		Overpriced,
		/// There are no cores available.
		Unavailable,
		/// The sale limit has been reached.
		SoldOut,
		/// The renewal operation is not valid at the current time (it may become valid in the next
		/// sale).
		WrongTime,
		/// Invalid attempt to renew.
		NotAllowed,
		/// This pallet has not yet been initialized.
		Uninitialized,
		/// The purchase cannot happen yet as the sale period is yet to begin.
		TooEarly,
		/// There is no work to be done.
		NothingToDo,
		/// The maximum amount of reservations has already been reached.
		TooManyReservations,
		/// The maximum amount of leases has already been reached.
		TooManyLeases,
		/// The lease does not exist.
		LeaseNotFound,
		/// The revenue for the Instantaneous Core Sales of this period is not (yet) known and thus
		/// this operation cannot proceed.
		UnknownRevenue,
		/// The identified contribution to the Instantaneous Core Pool is unknown.
		UnknownContribution,
		/// The workload assigned for renewal is incomplete. This is unexpected and indicates a
		/// logic error.
		IncompleteAssignment,
		/// An item cannot be dropped because it is still valid.
		StillValid,
		/// The history item does not exist.
		NoHistory,
		/// No reservation of the given index exists.
		UnknownReservation,
		/// The renewal record cannot be found.
		UnknownRenewal,
		/// The lease expiry time has already passed.
		AlreadyExpired,
		/// The configuration could not be applied because it is invalid.
		InvalidConfig,
		/// The revenue must be claimed for 1 or more timeslices.
		NoClaimTimeslices,
		/// The caller doesn't have the permission to enable or disable auto-renewal.
		NoPermission,
		/// We reached the limit for auto-renewals.
		TooManyAutoRenewals,
		/// Only cores which are assigned to a task can be auto-renewed.
		NonTaskAutoRenewal,
		/// Failed to get the sovereign account of a task.
		SovereignAccountNotFound,
		/// Attempted to disable auto-renewal for a core that didn't have it enabled.
		AutoRenewalNotEnabled,
		/// Attempted to force remove an assignment that doesn't exist.
		AssignmentNotFound,
		/// Needed to prevent spam attacks.The amount of credits the user attempted to purchase is
		/// below `T::MinimumCreditPurchase`.
		CreditPurchaseTooSmall,
		/// The renewable workload of the core does not include the given task.
		TaskNotInWorkload,
	}

	#[derive(frame_support::DefaultNoBound)]
	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		#[serde(skip)]
		pub _config: core::marker::PhantomData<T>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			frame_system::Pallet::<T>::inc_providers(&Pallet::<T>::account_id());
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_now: BlockNumberFor<T>) -> Weight {
			Self::do_tick()
		}
	}

	#[pallet::call(weight(<T as Config>::WeightInfo))]
	impl<T: Config> Pallet<T> {
		/// Configure the pallet.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `config`: The configuration for this pallet.
		#[pallet::call_index(0)]
		pub fn configure(
			origin: OriginFor<T>,
			config: ConfigRecordOf<T>,
		) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_configure(config)?;
			Ok(Pays::No.into())
		}

		/// Reserve a core for a workload.
		///
		/// The workload will be given a reservation, but two sale period boundaries must pass
		/// before the core is actually assigned.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `workload`: The workload which should be permanently placed on a core.
		#[pallet::call_index(1)]
		pub fn reserve(origin: OriginFor<T>, workload: Schedule) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_reserve(workload)?;
			Ok(Pays::No.into())
		}

		/// Cancel a reservation for a workload.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `item_index`: The index of the reservation. Usually this will also be the index of the
		///   core on which the reservation has been scheduled. However, it is possible that if
		///   other cores are reserved or unreserved in the same sale rotation that they won't
		///   correspond, so it's better to look up the core properly in the `Reservations` storage.
		#[pallet::call_index(2)]
		pub fn unreserve(origin: OriginFor<T>, item_index: u32) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_unreserve(item_index)?;
			Ok(Pays::No.into())
		}

		/// Reserve a core for a single task workload for a limited period.
		///
		/// In the interlude and sale period where Bulk Coretime is sold for the period immediately
		/// after `until`, then the same workload may be renewed.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `task`: The workload which should be placed on a core.
		/// - `until`: The timeslice now earlier than which `task` should be placed as a workload on
		///   a core.
		#[pallet::call_index(3)]
		pub fn set_lease(
			origin: OriginFor<T>,
			task: TaskId,
			until: Timeslice,
		) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_set_lease(task, until)?;
			Ok(Pays::No.into())
		}

		/// Begin the Bulk Coretime sales rotation.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `end_price`: The price after the leadin period of Bulk Coretime in the first sale.
		/// - `extra_cores`: Number of extra cores that should be requested on top of the cores
		///   required for `Reservations` and `Leases`.
		///
		/// This will call [`Self::request_core_count`] internally to set the correct core count on
		/// the relay chain.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::start_sales(
			T::MaxLeasedCores::get() + T::MaxReservedCores::get() + *extra_cores as u32
		))]
		pub fn start_sales(
			origin: OriginFor<T>,
			end_price: BalanceOf<T>,
			extra_cores: CoreIndex,
		) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_start_sales(end_price, extra_cores)?;
			Ok(Pays::No.into())
		}

		/// Purchase Bulk Coretime in the ongoing Sale.
		///
		/// - `origin`: Must be a Signed origin with at least enough funds to pay the current price
		///   of Bulk Coretime.
		/// - `price_limit`: An amount no more than which should be paid.
		#[pallet::call_index(5)]
		pub fn purchase(
			origin: OriginFor<T>,
			price_limit: BalanceOf<T>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			Self::do_purchase(who, price_limit)?;
			Ok(Pays::No.into())
		}

		/// Renew Bulk Coretime in the ongoing Sale or its prior Interlude Period.
		///
		/// - `origin`: Must be a Signed origin with at least enough funds to pay the renewal price
		///   of the core.
		/// - `core`: The core which should be renewed.
		#[pallet::call_index(6)]
		pub fn renew(origin: OriginFor<T>, core: CoreIndex) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			Self::do_renew(who, core)?;
			Ok(Pays::No.into())
		}

		/// Transfer a Bulk Coretime Region to a new owner.
		///
		/// - `origin`: Must be a Signed origin of the account which owns the Region `region_id`.
		/// - `region_id`: The Region whose ownership should change.
		/// - `new_owner`: The new owner for the Region.
		#[pallet::call_index(7)]
		pub fn transfer(
			origin: OriginFor<T>,
			region_id: RegionId,
			new_owner: T::AccountId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_transfer(region_id, Some(who), new_owner)?;
			Ok(())
		}

		/// Split a Bulk Coretime Region into two non-overlapping Regions at a particular time into
		/// the region.
		///
		/// - `origin`: Must be a Signed origin of the account which owns the Region `region_id`.
		/// - `region_id`: The Region which should be partitioned into two non-overlapping Regions.
		/// - `pivot`: The offset in time into the Region at which to make the split.
		#[pallet::call_index(8)]
		pub fn partition(
			origin: OriginFor<T>,
			region_id: RegionId,
			pivot: Timeslice,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_partition(region_id, Some(who), pivot)?;
			Ok(())
		}

		/// Split a Bulk Coretime Region into two wholly-overlapping Regions with complementary
		/// interlace masks which together make up the original Region's interlace mask.
		///
		/// - `origin`: Must be a Signed origin of the account which owns the Region `region_id`.
		/// - `region_id`: The Region which should become two interlaced Regions of incomplete
		///   regularity.
		/// - `pivot`: The interlace mask of one of the two new regions (the other is its partial
		///   complement).
		#[pallet::call_index(9)]
		pub fn interlace(
			origin: OriginFor<T>,
			region_id: RegionId,
			pivot: CoreMask,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_interlace(region_id, Some(who), pivot)?;
			Ok(())
		}

		/// Assign a Bulk Coretime Region to a task.
		///
		/// - `origin`: Must be a Signed origin of the account which owns the Region `region_id`.
		/// - `region_id`: The Region which should be assigned to the task.
		/// - `task`: The task to assign.
		/// - `finality`: Indication of whether this assignment is final (in which case it may be
		///   eligible for renewal) or provisional (in which case it may be manipulated and/or
		/// reassigned at a later stage).
		#[pallet::call_index(10)]
		pub fn assign(
			origin: OriginFor<T>,
			region_id: RegionId,
			task: TaskId,
			finality: Finality,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			Self::do_assign(region_id, Some(who), task, finality)?;
			Ok(if finality == Finality::Final { Pays::No } else { Pays::Yes }.into())
		}

		/// Place a Bulk Coretime Region into the Instantaneous Coretime Pool.
		///
		/// - `origin`: Must be a Signed origin of the account which owns the Region `region_id`.
		/// - `region_id`: The Region which should be assigned to the Pool.
		/// - `payee`: The account which is able to collect any revenue due for the usage of this
		///   Coretime.
		#[pallet::call_index(11)]
		pub fn pool(
			origin: OriginFor<T>,
			region_id: RegionId,
			payee: T::AccountId,
			finality: Finality,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			Self::do_pool(region_id, Some(who), payee, finality)?;
			Ok(if finality == Finality::Final { Pays::No } else { Pays::Yes }.into())
		}

		/// Claim the revenue owed from inclusion in the Instantaneous Coretime Pool.
		///
		/// - `origin`: Must be a Signed origin.
		/// - `region_id`: The Region which was assigned to the Pool.
		/// - `max_timeslices`: The maximum number of timeslices which should be processed. This
		///   must be greater than 0. This may affect the weight of the call but should be ideally
		///   made equivalent to the length of the Region `region_id`. If less, further dispatches
		///   will be required with the same `region_id` to claim revenue for the remainder.
		#[pallet::call_index(12)]
		#[pallet::weight(T::WeightInfo::claim_revenue(*max_timeslices))]
		pub fn claim_revenue(
			origin: OriginFor<T>,
			region_id: RegionId,
			max_timeslices: Timeslice,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			Self::do_claim_revenue(region_id, max_timeslices)?;
			Ok(Pays::No.into())
		}

		/// Purchase credit for use in the Instantaneous Coretime Pool.
		///
		/// - `origin`: Must be a Signed origin able to pay at least `amount`.
		/// - `amount`: The amount of credit to purchase.
		/// - `beneficiary`: The account on the Relay-chain which controls the credit (generally
		///   this will be the collator's hot wallet).
		#[pallet::call_index(13)]
		pub fn purchase_credit(
			origin: OriginFor<T>,
			amount: BalanceOf<T>,
			beneficiary: RelayAccountIdOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Self::do_purchase_credit(who, amount, beneficiary)?;
			Ok(())
		}

		/// Drop an expired Region from the chain.
		///
		/// - `origin`: Can be any kind of origin.
		/// - `region_id`: The Region which has expired.
		#[pallet::call_index(14)]
		pub fn drop_region(
			_origin: OriginFor<T>,
			region_id: RegionId,
		) -> DispatchResultWithPostInfo {
			Self::do_drop_region(region_id)?;
			Ok(Pays::No.into())
		}

		/// Drop an expired Instantaneous Pool Contribution record from the chain.
		///
		/// - `origin`: Can be any kind of origin.
		/// - `region_id`: The Region identifying the Pool Contribution which has expired.
		#[pallet::call_index(15)]
		pub fn drop_contribution(
			_origin: OriginFor<T>,
			region_id: RegionId,
		) -> DispatchResultWithPostInfo {
			Self::do_drop_contribution(region_id)?;
			Ok(Pays::No.into())
		}

		/// Drop an expired Instantaneous Pool History record from the chain.
		///
		/// - `origin`: Can be any kind of origin.
		/// - `region_id`: The time of the Pool History record which has expired.
		#[pallet::call_index(16)]
		pub fn drop_history(_origin: OriginFor<T>, when: Timeslice) -> DispatchResultWithPostInfo {
			Self::do_drop_history(when)?;
			Ok(Pays::No.into())
		}

		/// Drop an expired Allowed Renewal record from the chain.
		///
		/// - `origin`: Can be any kind of origin.
		/// - `core`: The core to which the expired renewal refers.
		/// - `when`: The timeslice to which the expired renewal refers. This must have passed.
		#[pallet::call_index(17)]
		pub fn drop_renewal(
			_origin: OriginFor<T>,
			core: CoreIndex,
			when: Timeslice,
		) -> DispatchResultWithPostInfo {
			Self::do_drop_renewal(core, when)?;
			Ok(Pays::No.into())
		}

		/// Request a change to the number of cores available for scheduling work.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `core_count`: The desired number of cores to be made available.
		#[pallet::call_index(18)]
		#[pallet::weight(T::WeightInfo::request_core_count((*core_count).into()))]
		pub fn request_core_count(origin: OriginFor<T>, core_count: CoreIndex) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_request_core_count(core_count)?;
			Ok(())
		}

		#[pallet::call_index(19)]
		#[pallet::weight(T::WeightInfo::notify_core_count())]
		pub fn notify_core_count(origin: OriginFor<T>, core_count: CoreIndex) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_notify_core_count(core_count)?;
			Ok(())
		}

		#[pallet::call_index(20)]
		#[pallet::weight(T::WeightInfo::notify_revenue())]
		pub fn notify_revenue(
			origin: OriginFor<T>,
			revenue: OnDemandRevenueRecordOf<T>,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_notify_revenue(revenue)?;
			Ok(())
		}

		/// Extrinsic for enabling auto renewal.
		///
		/// Callable by the sovereign account of the task on the specified core. This account
		/// will be charged at the start of every bulk period for renewing core time.
		///
		/// - `origin`: Must be the sovereign account of the task
		/// - `core`: The core to which the task to be renewed is currently assigned.
		/// - `task`: The task for which we want to enable auto renewal.
		/// - `workload_end_hint`: should be used when enabling auto-renewal for a core that is not
		///   expiring in the upcoming bulk period (e.g., due to holding a lease) since it would be
		///   inefficient to look up when the core expires to schedule the next renewal. Also used
		///   when the core is expiring with another task's workload, in which case it must point at
		///   the task's own renewal record.
		#[pallet::call_index(21)]
		#[pallet::weight(T::WeightInfo::enable_auto_renew())]
		pub fn enable_auto_renew(
			origin: OriginFor<T>,
			core: CoreIndex,
			task: TaskId,
			workload_end_hint: Option<Timeslice>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let sovereign_account = T::SovereignAccountOf::maybe_convert(task)
				.ok_or(Error::<T>::SovereignAccountNotFound)?;
			// Only the sovereign account of a task can enable auto renewal for its own core.
			ensure!(who == sovereign_account, Error::<T>::NoPermission);

			Self::do_enable_auto_renew(sovereign_account, core, task, workload_end_hint)?;
			Ok(())
		}

		/// Extrinsic for disabling auto renewal.
		///
		/// Callable by the sovereign account of the task on the specified core.
		///
		/// - `origin`: Must be the sovereign account of the task.
		/// - `core`: The core for which we want to disable auto renewal.
		/// - `task`: The task for which we want to disable auto renewal.
		#[pallet::call_index(22)]
		#[pallet::weight(T::WeightInfo::disable_auto_renew())]
		pub fn disable_auto_renew(
			origin: OriginFor<T>,
			core: CoreIndex,
			task: TaskId,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let sovereign_account = T::SovereignAccountOf::maybe_convert(task)
				.ok_or(Error::<T>::SovereignAccountNotFound)?;
			// Only the sovereign account of the task can disable auto-renewal.
			ensure!(who == sovereign_account, Error::<T>::NoPermission);

			Self::do_disable_auto_renew(core, task)?;

			Ok(())
		}

		/// Reserve a core for a workload immediately.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `workload`: The workload which should be permanently placed on a core starting
		///   immediately.
		/// - `core`: The core to which the assignment should be made until the reservation takes
		///   effect. It is left to the caller to either add this new core or reassign any other
		///   tasks to this existing core.
		///
		/// This reserves the workload and then injects the workload into the Workplan for the next
		/// two sale periods. This overwrites any existing assignments for this core at the start of
		/// the next sale period.
		#[pallet::call_index(23)]
		pub fn force_reserve(
			origin: OriginFor<T>,
			workload: Schedule,
			core: CoreIndex,
		) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_force_reserve(workload, core)?;
			Ok(Pays::No.into())
		}

		/// Remove a lease.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `task`: The task id of the lease which should be removed.
		#[pallet::call_index(24)]
		pub fn remove_lease(origin: OriginFor<T>, task: TaskId) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_remove_lease(task)
		}

		/// Remove an assignment from the Workplan.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `region_id`: The Region to be removed from the workplan.
		#[pallet::call_index(26)]
		pub fn remove_assignment(origin: OriginFor<T>, region_id: RegionId) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_remove_assignment(region_id)
		}

		/// Forcefully remove a potential renewal record from chain.
		///
		/// Note that only the specified potential renewal will be removed while any related auto
		/// renewals will stay intact and will fail.
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `core`: Core which the target potential renewal record refers to.
		/// - `when`: Timeslice which the target potential renewal record refers to.
		#[pallet::call_index(27)]
		pub fn remove_potential_renewal(
			origin: OriginFor<T>,
			core: CoreIndex,
			when: Timeslice,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_remove_potential_renewal(core, when)
		}

		/// Transfer a Bulk Coretime Region to a new owner, ignoring the previous owner.
		///
		/// This can also be used to recover regions that have been "burned" (e.g., from an
		/// XCM reserve transfer).
		///
		/// - `origin`: Must be Root or pass `AdminOrigin`.
		/// - `region_id`: The Region whose ownership should change.
		/// - `new_owner`: The new owner for the Region.
		#[pallet::call_index(28)]
		pub fn force_transfer(
			origin: OriginFor<T>,
			region_id: RegionId,
			new_owner: T::AccountId,
		) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_transfer(region_id, None, new_owner)?;
			Ok(())
		}

		#[pallet::call_index(99)]
		#[pallet::weight(T::WeightInfo::swap_leases())]
		pub fn swap_leases(origin: OriginFor<T>, id: TaskId, other: TaskId) -> DispatchResult {
			T::AdminOrigin::ensure_origin_or_root(origin)?;
			Self::do_swap_leases(id, other)?;
			Ok(())
		}
	}
}
