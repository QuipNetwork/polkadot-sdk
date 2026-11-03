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

use core::cmp;

use super::*;
use frame_support::{
	pallet_prelude::*,
	traits::{fungible::Mutate, tokens::Preservation::Expendable, DefensiveResult},
};
use sp_arithmetic::traits::{CheckedDiv, Saturating, Zero};
use sp_runtime::traits::{BlockNumberProvider, Convert};
use CompletionStatus::{Complete, Partial};

impl<T: Config> Pallet<T> {
	pub(crate) fn do_configure(config: ConfigRecordOf<T>) -> DispatchResult {
		config.validate().map_err(|()| Error::<T>::InvalidConfig)?;
		Configuration::<T>::put(config);
		Ok(())
	}

	pub(crate) fn do_request_core_count(core_count: CoreIndex) -> DispatchResult {
		T::Coretime::request_core_count(core_count);
		Self::deposit_event(Event::<T>::CoreCountRequested { core_count });
		Ok(())
	}

	pub(crate) fn do_notify_core_count(core_count: CoreIndex) -> DispatchResult {
		CoreCountInbox::<T>::put(core_count);
		Ok(())
	}

	pub(crate) fn do_reserve(workload: Schedule) -> DispatchResult {
		let mut r = Reservations::<T>::get();
		let index = r.len() as u32;
		r.try_push(workload.clone()).map_err(|_| Error::<T>::TooManyReservations)?;
		Reservations::<T>::put(r);
		Self::deposit_event(Event::<T>::ReservationMade { index, workload });
		Ok(())
	}

	pub(crate) fn do_unreserve(index: u32) -> DispatchResult {
		let mut r = Reservations::<T>::get();
		ensure!(index < r.len() as u32, Error::<T>::UnknownReservation);
		let workload = r.remove(index as usize);
		Reservations::<T>::put(r);
		Self::deposit_event(Event::<T>::ReservationCancelled { index, workload });
		Ok(())
	}

	pub(crate) fn do_force_reserve(workload: Schedule, core: CoreIndex) -> DispatchResult {
		// Sales must have started, otherwise reserve is equivalent.
		let sale = SaleInfo::<T>::get().ok_or(Error::<T>::NoSales)?;

		// Reserve - starts at second sale period boundary from now.
		Self::do_reserve(workload.clone())?;

		// Add to ForceReservations for dynamic core assignment in rotate_sale.
		ForceReservations::<T>::try_mutate(|r| {
			r.try_push(workload.clone()).map_err(|_| Error::<T>::TooManyReservations)
		})?;

		// Assign now until the next sale boundary unless the next timeslice is already the sale
		// boundary.
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let timeslice = status.last_committed_timeslice.saturating_add(1);
		if timeslice < sale.region_begin {
			Workplan::<T>::insert((timeslice, core), &workload);
		}

		Ok(())
	}

	pub(crate) fn do_set_lease(task: TaskId, until: Timeslice) -> DispatchResult {
		let mut r = Leases::<T>::get();
		ensure!(until > Self::current_timeslice(), Error::<T>::AlreadyExpired);
		r.try_push(LeaseRecordItem { until, task })
			.map_err(|_| Error::<T>::TooManyLeases)?;
		Leases::<T>::put(r);
		Self::deposit_event(Event::<T>::Leased { until, task });
		Ok(())
	}

	pub(crate) fn do_remove_lease(task: TaskId) -> DispatchResult {
		let mut r = Leases::<T>::get();
		let i = r.iter().position(|lease| lease.task == task).ok_or(Error::<T>::LeaseNotFound)?;
		r.remove(i);
		Leases::<T>::put(r);
		Self::deposit_event(Event::<T>::LeaseRemoved { task });
		Ok(())
	}

	pub(crate) fn do_start_sales(
		end_price: BalanceOf<T>,
		extra_cores: CoreIndex,
	) -> DispatchResult {
		let config = Configuration::<T>::get().ok_or(Error::<T>::Uninitialized)?;

		// Determine the core count
		let core_count = Leases::<T>::decode_len().unwrap_or(0) as CoreIndex +
			Reservations::<T>::decode_len().unwrap_or(0) as CoreIndex +
			extra_cores;

		Self::do_request_core_count(core_count)?;

		let commit_timeslice = Self::latest_timeslice_ready_to_commit(&config);
		let status = StatusRecord {
			core_count,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: commit_timeslice.saturating_sub(1),
			last_timeslice: Self::current_timeslice(),
		};
		let now = RCBlockNumberProviderOf::<T::Coretime>::current_block_number();
		// Imaginary old sale for bootstrapping the first actual sale:
		let old_sale = SaleInfoRecord {
			sale_start: now,
			leadin_length: Zero::zero(),
			end_price,
			sellout_price: None,
			region_begin: commit_timeslice,
			region_end: commit_timeslice.saturating_add(config.region_length),
			first_core: 0,
			ideal_cores_sold: 0,
			cores_offered: 0,
			cores_sold: 0,
			sale_index: 0,
		};
		Self::deposit_event(Event::<T>::SalesStarted { price: end_price, core_count });
		Self::rotate_sale(old_sale, &config, &status);
		Status::<T>::put(&status);
		Ok(())
	}

	pub(crate) fn do_purchase(
		who: T::AccountId,
		price_limit: BalanceOf<T>,
	) -> Result<RegionId, DispatchError> {
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let mut sale = SaleInfo::<T>::get().ok_or(Error::<T>::NoSales)?;
		Self::ensure_cores_for_sale(&status, &sale)?;

		let now = RCBlockNumberProviderOf::<T::Coretime>::current_block_number();
		ensure!(now > sale.sale_start, Error::<T>::TooEarly);
		let price = Self::sale_price(&sale, now);
		ensure!(price_limit >= price, Error::<T>::Overpriced);

		let core = Self::purchase_core(&who, price, &mut sale)?;

		SaleInfo::<T>::put(&sale);
		let id = Self::issue(
			core,
			sale.region_begin,
			CoreMask::complete(),
			sale.region_end,
			Some(who.clone()),
			Some(price),
		);
		let duration = sale.region_end.saturating_sub(sale.region_begin);
		Self::deposit_event(Event::Purchased { who, region_id: id, price, duration });
		Ok(id)
	}

	/// Must be called on a core in `PotentialRenewals` whose value is a timeslice equal to the
	/// current sale status's `region_end`.
	pub(crate) fn do_renew(who: T::AccountId, core: CoreIndex) -> Result<CoreIndex, DispatchError> {
		let config = Configuration::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let mut sale = SaleInfo::<T>::get().ok_or(Error::<T>::NoSales)?;
		Self::ensure_cores_for_sale(&status, &sale)?;

		let renewal_id = PotentialRenewalId { core, when: sale.region_begin };
		let record = PotentialRenewals::<T>::get(renewal_id).ok_or(Error::<T>::NotAllowed)?;
		let workload =
			record.completion.drain_complete().ok_or(Error::<T>::IncompleteAssignment)?;

		let old_core = core;

		let core = Self::purchase_core(&who, record.price, &mut sale)?;

		Self::deposit_event(Event::Renewed {
			who,
			old_core,
			core,
			price: record.price,
			begin: sale.region_begin,
			duration: sale.region_end.saturating_sub(sale.region_begin),
			workload: workload.clone(),
		});

		Workplan::<T>::insert((sale.region_begin, core), &workload);

		let begin = sale.region_end;
		let end_price = sale.end_price;
		// Renewals should never be priced lower than the current `end_price`:
		let price_cap = cmp::max(record.price + config.renewal_bump * record.price, end_price);
		let now = RCBlockNumberProviderOf::<T::Coretime>::current_block_number();
		let price = Self::sale_price(&sale, now).min(price_cap);
		log::debug!(
			"Renew with: sale price: {:?}, price cap: {:?}, old price: {:?}",
			price,
			price_cap,
			record.price
		);
		let new_record = PotentialRenewalRecord { price, completion: Complete(workload) };
		PotentialRenewals::<T>::remove(renewal_id);
		PotentialRenewals::<T>::insert(PotentialRenewalId { core, when: begin }, &new_record);
		SaleInfo::<T>::put(&sale);
		if let Some(workload) = new_record.completion.drain_complete() {
			log::debug!("Recording renewable price for next run: {:?}", price);
			Self::deposit_event(Event::Renewable { core, price, begin, workload });
		}
		Ok(core)
	}

	pub(crate) fn do_transfer(
		region_id: RegionId,
		maybe_check_owner: Option<T::AccountId>,
		new_owner: T::AccountId,
	) -> Result<(), Error<T>> {
		let mut region = Regions::<T>::get(&region_id).ok_or(Error::<T>::UnknownRegion)?;

		if let Some(check_owner) = maybe_check_owner {
			ensure!(Some(check_owner) == region.owner, Error::<T>::NotOwner);
		}

		let old_owner = region.owner;
		region.owner = Some(new_owner);
		Regions::<T>::insert(&region_id, &region);
		let duration = region.end.saturating_sub(region_id.begin);
		Self::deposit_event(Event::Transferred {
			region_id,
			old_owner,
			owner: region.owner,
			duration,
		});

		Ok(())
	}

	pub(crate) fn do_partition(
		region_id: RegionId,
		maybe_check_owner: Option<T::AccountId>,
		pivot_offset: Timeslice,
	) -> Result<(RegionId, RegionId), Error<T>> {
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let mut region = Regions::<T>::get(&region_id).ok_or(Error::<T>::UnknownRegion)?;

		if let Some(check_owner) = maybe_check_owner {
			ensure!(Some(check_owner) == region.owner, Error::<T>::NotOwner);
		}
		let pivot = region_id.begin.saturating_add(pivot_offset);
		ensure!(pivot < region.end, Error::<T>::PivotTooLate);
		ensure!(pivot > region_id.begin, Error::<T>::PivotTooEarly);

		region.paid = None;
		let new_region_ids = (region_id, RegionId { begin: pivot, ..region_id });

		// Remove this region from the pool in case it has been assigned provisionally. If we get
		// this far then it is still in `Regions` and thus could only have been pooled
		// provisionally.
		Self::force_unpool_region(region_id, &region, &status);

		// Overwrite the previous region with its new end and create a new region for the second
		// part of the partition.
		Regions::<T>::insert(&new_region_ids.0, &RegionRecord { end: pivot, ..region.clone() });
		Regions::<T>::insert(&new_region_ids.1, &region);
		Self::deposit_event(Event::Partitioned { old_region_id: region_id, new_region_ids });

		Ok(new_region_ids)
	}

	pub(crate) fn do_interlace(
		region_id: RegionId,
		maybe_check_owner: Option<T::AccountId>,
		pivot: CoreMask,
	) -> Result<(RegionId, RegionId), Error<T>> {
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let region = Regions::<T>::get(&region_id).ok_or(Error::<T>::UnknownRegion)?;

		if let Some(check_owner) = maybe_check_owner {
			ensure!(Some(check_owner) == region.owner, Error::<T>::NotOwner);
		}

		ensure!((pivot & !region_id.mask).is_void(), Error::<T>::ExteriorPivot);
		ensure!(!pivot.is_void(), Error::<T>::VoidPivot);
		ensure!(pivot != region_id.mask, Error::<T>::CompletePivot);

		// Remove this region from the pool in case it has been assigned provisionally. If we get
		// this far then it is still in `Regions` and thus could only have been pooled
		// provisionally.
		Self::force_unpool_region(region_id, &region, &status);

		// The old region should be removed.
		Regions::<T>::remove(&region_id);

		let one = RegionId { mask: pivot, ..region_id };
		Regions::<T>::insert(&one, &region);
		let other = RegionId { mask: region_id.mask ^ pivot, ..region_id };
		Regions::<T>::insert(&other, &region);

		let new_region_ids = (one, other);
		Self::deposit_event(Event::Interlaced { old_region_id: region_id, new_region_ids });
		Ok(new_region_ids)
	}

	pub(crate) fn do_assign(
		region_id: RegionId,
		maybe_check_owner: Option<T::AccountId>,
		target: TaskId,
		finality: Finality,
	) -> Result<(), Error<T>> {
		let config = Configuration::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;

		if let Some((region_id, region)) = Self::utilize(region_id, maybe_check_owner, finality)? {
			let workplan_key = (region_id.begin, region_id.core);
			let mut workplan = Workplan::<T>::get(&workplan_key).unwrap_or_default();

			// Remove this region from the pool in case it has been assigned provisionally. If we
			// get this far then it is still in `Regions` and thus could only have been pooled
			// provisionally.
			Self::force_unpool_region(region_id, &region, &status);

			// Ensure no previous allocations exist.
			workplan.retain(|i| (i.mask & region_id.mask).is_void());
			if workplan
				.try_push(ScheduleItem {
					mask: region_id.mask,
					assignment: CoreAssignment::Task(target),
				})
				.is_ok()
			{
				Workplan::<T>::insert(&workplan_key, &workplan);
			}

			let duration = region.end.saturating_sub(region_id.begin);
			if duration == config.region_length && finality == Finality::Final {
				if let Some(price) = region.paid {
					let renewal_id = PotentialRenewalId { core: region_id.core, when: region.end };
					let assigned = match PotentialRenewals::<T>::get(renewal_id) {
						Some(PotentialRenewalRecord { completion: Partial(w), price: p })
							if price == p =>
						{
							w
						},
						_ => CoreMask::void(),
					} | region_id.mask;
					let workload =
						if assigned.is_complete() { Complete(workplan) } else { Partial(assigned) };
					let record = PotentialRenewalRecord { price, completion: workload };
					// Note: This entry alone does not yet actually allow renewals (the completion
					// status has to be complete for `do_renew` to accept it).
					PotentialRenewals::<T>::insert(&renewal_id, &record);
					if let Some(workload) = record.completion.drain_complete() {
						Self::deposit_event(Event::Renewable {
							core: region_id.core,
							price,
							begin: region.end,
							workload,
						});
					}
				}
			}
			Self::deposit_event(Event::Assigned { region_id, task: target, duration });
		}
		Ok(())
	}

	pub(crate) fn do_remove_assignment(region_id: RegionId) -> DispatchResult {
		let workplan_key = (region_id.begin, region_id.core);
		ensure!(Workplan::<T>::contains_key(&workplan_key), Error::<T>::AssignmentNotFound);
		Workplan::<T>::remove(&workplan_key);
		Self::deposit_event(Event::<T>::AssignmentRemoved { region_id });
		Ok(())
	}

	pub(crate) fn do_pool(
		region_id: RegionId,
		maybe_check_owner: Option<T::AccountId>,
		payee: T::AccountId,
		finality: Finality,
	) -> Result<(), Error<T>> {
		if let Some((region_id, region)) = Self::utilize(region_id, maybe_check_owner, finality)? {
			let workplan_key = (region_id.begin, region_id.core);
			let mut workplan = Workplan::<T>::get(&workplan_key).unwrap_or_default();
			let duration = region.end.saturating_sub(region_id.begin);
			if workplan
				.try_push(ScheduleItem { mask: region_id.mask, assignment: CoreAssignment::Pool })
				.is_ok()
			{
				Workplan::<T>::insert(&workplan_key, &workplan);
				let size = region_id.mask.count_ones() as i32;
				InstaPoolIo::<T>::mutate(region_id.begin, |a| a.private.saturating_accrue(size));
				InstaPoolIo::<T>::mutate(region.end, |a| a.private.saturating_reduce(size));
				let record = ContributionRecord { length: duration, payee };
				InstaPoolContribution::<T>::insert(&region_id, record);
			}

			Self::deposit_event(Event::Pooled { region_id, duration });
		}
		Ok(())
	}

	pub(crate) fn do_claim_revenue(
		mut region: RegionId,
		max_timeslices: Timeslice,
	) -> DispatchResult {
		ensure!(max_timeslices > 0, Error::<T>::NoClaimTimeslices);
		let mut contribution =
			InstaPoolContribution::<T>::take(region).ok_or(Error::<T>::UnknownContribution)?;
		let contributed_parts = region.mask.count_ones();

		Self::deposit_event(Event::RevenueClaimBegun { region, max_timeslices });

		let mut payout = BalanceOf::<T>::zero();
		let last = region.begin + contribution.length.min(max_timeslices);
		for r in region.begin..last {
			region.begin = r + 1;
			contribution.length.saturating_dec();

			let Some(mut pool_record) = InstaPoolHistory::<T>::get(r) else { continue };
			let Some(total_payout) = pool_record.maybe_payout else { break };
			let p = total_payout
				.saturating_mul(contributed_parts.into())
				.checked_div(&pool_record.private_contributions.into())
				.unwrap_or_default();

			payout.saturating_accrue(p);
			pool_record.private_contributions.saturating_reduce(contributed_parts);

			let remaining_payout = total_payout.saturating_sub(p);
			if !remaining_payout.is_zero() && pool_record.private_contributions > 0 {
				pool_record.maybe_payout = Some(remaining_payout);
				InstaPoolHistory::<T>::insert(r, &pool_record);
			} else {
				InstaPoolHistory::<T>::remove(r);
			}
			if !p.is_zero() {
				Self::deposit_event(Event::RevenueClaimItem { when: r, amount: p });
			}
		}

		if contribution.length > 0 {
			InstaPoolContribution::<T>::insert(region, &contribution);
		}
		T::Currency::transfer(&Self::account_id(), &contribution.payee, payout, Expendable)
			.defensive_ok();
		let next = if last < region.begin + contribution.length { Some(region) } else { None };
		Self::deposit_event(Event::RevenueClaimPaid {
			who: contribution.payee,
			amount: payout,
			next,
		});
		Ok(())
	}

	pub(crate) fn do_purchase_credit(
		who: T::AccountId,
		amount: BalanceOf<T>,
		beneficiary: RelayAccountIdOf<T>,
	) -> DispatchResult {
		ensure!(amount >= T::MinimumCreditPurchase::get(), Error::<T>::CreditPurchaseTooSmall);
		T::Currency::transfer(&who, &Self::account_id(), amount, Expendable)?;
		let rc_amount = T::ConvertBalance::convert(amount);
		T::Coretime::credit_account(beneficiary.clone(), rc_amount);
		Self::deposit_event(Event::<T>::CreditPurchased { who, beneficiary, amount });
		Ok(())
	}

	pub(crate) fn do_drop_region(region_id: RegionId) -> DispatchResult {
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let region = Regions::<T>::get(&region_id).ok_or(Error::<T>::UnknownRegion)?;
		ensure!(status.last_committed_timeslice >= region.end, Error::<T>::StillValid);

		Regions::<T>::remove(&region_id);
		let duration = region.end.saturating_sub(region_id.begin);
		Self::deposit_event(Event::RegionDropped { region_id, duration });
		Ok(())
	}

	pub(crate) fn do_drop_contribution(region_id: RegionId) -> DispatchResult {
		let config = Configuration::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let contrib =
			InstaPoolContribution::<T>::get(&region_id).ok_or(Error::<T>::UnknownContribution)?;
		let end = region_id.begin.saturating_add(contrib.length);
		ensure!(
			status.last_timeslice >= end.saturating_add(config.contribution_timeout),
			Error::<T>::StillValid
		);
		InstaPoolContribution::<T>::remove(region_id);
		Self::deposit_event(Event::ContributionDropped { region_id });
		Ok(())
	}

	pub(crate) fn do_drop_history(when: Timeslice) -> DispatchResult {
		let config = Configuration::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		ensure!(
			status.last_timeslice > when.saturating_add(config.contribution_timeout),
			Error::<T>::StillValid
		);
		let record = InstaPoolHistory::<T>::take(when).ok_or(Error::<T>::NoHistory)?;
		if let Some(payout) = record.maybe_payout {
			let _ = Self::charge(&Self::account_id(), payout);
		}
		let revenue = record.maybe_payout.unwrap_or_default();
		Self::deposit_event(Event::HistoryDropped { when, revenue });
		Ok(())
	}

	pub(crate) fn do_drop_renewal(core: CoreIndex, when: Timeslice) -> DispatchResult {
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		ensure!(status.last_committed_timeslice >= when, Error::<T>::StillValid);
		let id = PotentialRenewalId { core, when };
		ensure!(PotentialRenewals::<T>::contains_key(id), Error::<T>::UnknownRenewal);
		PotentialRenewals::<T>::remove(id);
		Self::deposit_event(Event::PotentialRenewalDropped { core, when });
		Ok(())
	}

	pub(crate) fn do_notify_revenue(revenue: OnDemandRevenueRecordOf<T>) -> DispatchResult {
		RevenueInbox::<T>::put(revenue);
		Ok(())
	}

	pub(crate) fn do_swap_leases(id: TaskId, other: TaskId) -> DispatchResult {
		let mut id_leases_count = 0;
		let mut other_leases_count = 0;
		Leases::<T>::mutate(|leases| {
			leases.iter_mut().for_each(|lease| {
				if lease.task == id {
					lease.task = other;
					id_leases_count += 1;
				} else if lease.task == other {
					lease.task = id;
					other_leases_count += 1;
				}
			})
		});
		Ok(())
	}

	pub(crate) fn do_enable_auto_renew(
		sovereign_account: T::AccountId,
		core: CoreIndex,
		task: TaskId,
		workload_end_hint: Option<Timeslice>,
	) -> DispatchResult {
		let sale = SaleInfo::<T>::get().ok_or(Error::<T>::NoSales)?;
		let mut core = core;

		// Check if the core is expiring in the next bulk period with the task's own workload;
		// if so, we will renew it now.
		//
		// A core index can be assigned a different workload each bulk period, so the core could
		// instead be expiring with another task's workload, which `task` must not pay to renew.
		// In that case the `workload_end_hint` can still point to the task's own renewal record.
		let renewable_now =
			PotentialRenewals::<T>::get(PotentialRenewalId { core, when: sale.region_begin })
				.map_or(false, |record| record.completion.is_complete_and_contains_task(task));

		let next_renewal = if renewable_now {
			core = Self::do_renew(sovereign_account.clone(), core)?;
			// The next renewal is due when the period we just renewed for ends.
			sale.region_end
		} else if let Some(workload_end) = workload_end_hint {
			let record =
				PotentialRenewals::<T>::get(PotentialRenewalId { core, when: workload_end })
					.ok_or(Error::<T>::NotAllowed)?;
			let workload = record.completion.complete().ok_or(Error::<T>::IncompleteAssignment)?;
			ensure!(
				workload.iter().any(|item| item.assignment == CoreAssignment::Task(task)),
				Error::<T>::TaskNotInWorkload
			);
			workload_end
		} else {
			return Err(Error::<T>::TaskNotInWorkload.into());
		};

		// We are sorting auto renewals by `CoreIndex`.
		AutoRenewals::<T>::try_mutate(|renewals| {
			let pos = renewals
				.binary_search_by(|r: &AutoRenewalRecord| r.core.cmp(&core))
				.unwrap_or_else(|e| e);
			renewals.try_insert(pos, AutoRenewalRecord { core, task, next_renewal })
		})
		.map_err(|_| Error::<T>::TooManyAutoRenewals)?;

		Self::deposit_event(Event::AutoRenewalEnabled { core, task });
		Ok(())
	}

	pub(crate) fn do_disable_auto_renew(core: CoreIndex, task: TaskId) -> DispatchResult {
		AutoRenewals::<T>::try_mutate(|renewals| -> DispatchResult {
			let pos = renewals
				.binary_search_by(|r: &AutoRenewalRecord| r.core.cmp(&core))
				.map_err(|_| Error::<T>::AutoRenewalNotEnabled)?;

			let renewal_record = renewals.get(pos).ok_or(Error::<T>::AutoRenewalNotEnabled)?;

			ensure!(
				renewal_record.core == core && renewal_record.task == task,
				Error::<T>::NoPermission
			);
			renewals.remove(pos);
			Ok(())
		})?;

		Self::deposit_event(Event::AutoRenewalDisabled { core, task });
		Ok(())
	}

	pub(crate) fn do_remove_potential_renewal(core: CoreIndex, when: Timeslice) -> DispatchResult {
		let renewal_id = PotentialRenewalId { core, when };

		PotentialRenewals::<T>::take(renewal_id).ok_or(Error::<T>::UnknownRenewal)?;

		Self::deposit_event(Event::PotentialRenewalRemoved { core, timeslice: when });

		Ok(())
	}

	pub(crate) fn ensure_cores_for_sale(
		status: &StatusRecord,
		sale: &SaleInfoRecordOf<T>,
	) -> Result<(), DispatchError> {
		ensure!(sale.first_core < status.core_count, Error::<T>::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, Error::<T>::SoldOut);

		Ok(())
	}

	/// If there is an ongoing sale returns the current price of a core.
	pub fn current_price() -> Result<BalanceOf<T>, DispatchError> {
		let status = Status::<T>::get().ok_or(Error::<T>::Uninitialized)?;
		let sale = SaleInfo::<T>::get().ok_or(Error::<T>::NoSales)?;

		Self::ensure_cores_for_sale(&status, &sale)?;

		let now = RCBlockNumberProviderOf::<T::Coretime>::current_block_number();
		Ok(Self::sale_price(&sale, now))
	}
}
