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

use super::*;
use alloc::{vec, vec::Vec};
use frame_support::{pallet_prelude::*, traits::defensive_prelude::*, weights::WeightMeter};
use sp_arithmetic::traits::{One, SaturatedConversion, Saturating, Zero};
use sp_runtime::traits::{BlockNumberProvider, ConvertBack, MaybeConvert};
use CompletionStatus::Complete;

impl<T: Config> Pallet<T> {
	/// Attempt to tick things along.
	///
	/// This may do several things:
	/// - Processes notifications of the core count changing
	/// - Processes reports of Instantaneous Core Market Revenue
	/// - Commit a timeslice
	/// - Rotate the sale period
	/// - Request revenue information for a previous timeslice
	/// - Initialize an instantaneous core pool historical revenue record
	pub(crate) fn do_tick() -> Weight {
		let mut meter = WeightMeter::new();
		meter.consume(T::WeightInfo::do_tick_base());

		let (mut status, config) = match (Status::<T>::get(), Configuration::<T>::get()) {
			(Some(s), Some(c)) => (s, c),
			_ => return meter.consumed(),
		};

		if Self::process_core_count(&mut status) {
			meter.consume(T::WeightInfo::process_core_count(status.core_count.into()));
		}

		if Self::process_revenue() {
			meter.consume(T::WeightInfo::process_revenue());
		}

		if let Some(commit_timeslice) = Self::next_timeslice_to_commit(&config, &status) {
			status.last_committed_timeslice = commit_timeslice;
			if let Some(sale) = SaleInfo::<T>::get() {
				if commit_timeslice >= sale.region_begin {
					// Sale can be rotated.
					Self::rotate_sale(sale, &config, &status);
					meter.consume(T::WeightInfo::rotate_sale(status.core_count.into()));
				}
			}

			Self::process_pool(commit_timeslice, &mut status);
			meter.consume(T::WeightInfo::process_pool());

			let timeslice_period = T::TimeslicePeriod::get();
			let rc_begin = RelayBlockNumberOf::<T>::from(commit_timeslice) * timeslice_period;
			for core in 0..status.core_count {
				Self::process_core_schedule(commit_timeslice, rc_begin, core);
				meter.consume(T::WeightInfo::process_core_schedule());
			}
		}

		let current_timeslice = Self::current_timeslice();
		if status.last_timeslice < current_timeslice {
			status.last_timeslice.saturating_inc();
			let rc_block = T::TimeslicePeriod::get() * status.last_timeslice.into();
			T::Coretime::request_revenue_info_at(rc_block);
			meter.consume(T::WeightInfo::request_revenue_info_at());
			T::Coretime::on_new_timeslice(status.last_timeslice);
			meter.consume(T::WeightInfo::on_new_timeslice());
		}

		Status::<T>::put(&status);

		meter.consumed()
	}

	pub(crate) fn process_core_count(status: &mut StatusRecord) -> bool {
		if let Some(core_count) = CoreCountInbox::<T>::take() {
			status.core_count = core_count;
			Self::deposit_event(Event::<T>::CoreCountChanged { core_count });
			return true;
		}
		false
	}

	pub(crate) fn process_revenue() -> bool {
		let Some(OnDemandRevenueRecord { until, amount }) = RevenueInbox::<T>::take() else {
			return false;
		};
		let when: Timeslice =
			(until / T::TimeslicePeriod::get()).saturating_sub(One::one()).saturated_into();
		let mut revenue = T::ConvertBalance::convert_back(amount.clone());
		if revenue.is_zero() {
			Self::deposit_event(Event::<T>::HistoryDropped { when, revenue });
			InstaPoolHistory::<T>::remove(when);
			return true;
		}

		log::debug!(
			target: "pallet_broker::process_revenue",
			"Received {amount:?} from RC, converted into {revenue:?} revenue",
		);

		let mut r = InstaPoolHistory::<T>::get(when).unwrap_or_default();
		if r.maybe_payout.is_some() {
			Self::deposit_event(Event::<T>::HistoryIgnored { when, revenue });
			return true;
		}
		// Payout system InstaPool Cores.
		let total_contrib = r.system_contributions.saturating_add(r.private_contributions);
		let system_payout = if !total_contrib.is_zero() {
			let system_payout =
				revenue.saturating_mul(r.system_contributions.into()) / total_contrib.into();
			Self::charge(&Self::account_id(), system_payout).defensive_ok();
			revenue.saturating_reduce(system_payout);

			system_payout
		} else {
			Zero::zero()
		};

		log::debug!(
			target: "pallet_broker::process_revenue",
			"Charged {system_payout:?} for system payouts, {revenue:?} remaining for private contributions",
		);

		if !revenue.is_zero() && r.private_contributions > 0 {
			r.maybe_payout = Some(revenue);
			InstaPoolHistory::<T>::insert(when, &r);
			Self::deposit_event(Event::<T>::ClaimsReady {
				when,
				system_payout,
				private_payout: revenue,
			});
		} else {
			InstaPoolHistory::<T>::remove(when);
			Self::deposit_event(Event::<T>::HistoryDropped { when, revenue });
		}
		true
	}

	/// Begin selling for the next sale period.
	///
	/// Triggered by Relay-chain block number/timeslice.
	pub(crate) fn rotate_sale(
		old_sale: SaleInfoRecordOf<T>,
		config: &ConfigRecordOf<T>,
		status: &StatusRecord,
	) -> Option<()> {
		let now = RCBlockNumberProviderOf::<T::Coretime>::current_block_number();

		let pool_item =
			ScheduleItem { assignment: CoreAssignment::Pool, mask: CoreMask::complete() };
		let just_pool = Schedule::truncate_from(vec![pool_item]);

		// Clean up the old sale - we need to use up any unused cores by putting them into the
		// InstaPool.
		let mut old_pooled: SignedCoreMaskBitCount = 0;
		for i in old_sale.cores_sold..old_sale.cores_offered {
			old_pooled.saturating_accrue(80);
			Workplan::<T>::insert((old_sale.region_begin, old_sale.first_core + i), &just_pool);
		}
		InstaPoolIo::<T>::mutate(old_sale.region_begin, |r| r.system.saturating_accrue(old_pooled));
		InstaPoolIo::<T>::mutate(old_sale.region_end, |r| r.system.saturating_reduce(old_pooled));

		// Calculate the start price for the upcoming sale.
		let new_prices = T::PriceAdapter::adapt_price(SalePerformance::from_sale(&old_sale));

		log::debug!(
			"Rotated sale, new prices: {:?}, {:?}",
			new_prices.end_price,
			new_prices.target_price
		);

		// Set workload for the reserved (system, probably) workloads.
		let region_begin = old_sale.region_end;
		let region_end = region_begin + config.region_length;

		let mut first_core = 0;
		let mut total_pooled: SignedCoreMaskBitCount = 0;
		for schedule in Reservations::<T>::get().into_iter() {
			let parts: u32 = schedule
				.iter()
				.filter(|i| matches!(i.assignment, CoreAssignment::Pool))
				.map(|i| i.mask.count_ones())
				.sum();
			total_pooled.saturating_accrue(parts as i32);

			Workplan::<T>::insert((region_begin, first_core), &schedule);
			first_core.saturating_inc();
		}

		// Insert ForceReservations at the first free core from the old sale.
		let mut force_core = old_sale.first_core + old_sale.cores_sold;
		for schedule in ForceReservations::<T>::take() {
			if force_core >= status.core_count {
				Self::deposit_event(Event::<T>::ForceReservationFailed { schedule });
				continue;
			}
			Workplan::<T>::insert((old_sale.region_begin, force_core), &schedule);
			force_core.saturating_inc();
		}

		InstaPoolIo::<T>::mutate(region_begin, |r| r.system.saturating_accrue(total_pooled));
		InstaPoolIo::<T>::mutate(region_end, |r| r.system.saturating_reduce(total_pooled));

		let mut leases = Leases::<T>::get();
		// Can morph to a renewable as long as it's >=begin and <end.
		leases.retain(|&LeaseRecordItem { until, task }| {
			let mask = CoreMask::complete();
			let assignment = CoreAssignment::Task(task);
			let schedule = BoundedVec::truncate_from(vec![ScheduleItem { mask, assignment }]);
			Workplan::<T>::insert((region_begin, first_core), &schedule);
			// Will the lease expire at the end of the period?
			let expire = until < region_end;
			if expire {
				// last time for this one - make it renewable in the next sale.
				let renewal_id = PotentialRenewalId { core: first_core, when: region_end };
				let record = PotentialRenewalRecord {
					price: new_prices.target_price,
					completion: Complete(schedule),
				};
				PotentialRenewals::<T>::insert(renewal_id, &record);
				Self::deposit_event(Event::Renewable {
					core: first_core,
					price: new_prices.target_price,
					begin: region_end,
					workload: record.completion.drain_complete().unwrap_or_default(),
				});
				Self::deposit_event(Event::LeaseEnding { when: region_end, task });
			}

			first_core.saturating_inc();

			!expire
		});
		Leases::<T>::put(&leases);

		let max_possible_sales = status.core_count.saturating_sub(first_core);
		let limit_cores_offered = config.limit_cores_offered.unwrap_or(CoreIndex::max_value());
		let cores_offered = limit_cores_offered.min(max_possible_sales);
		let sale_start = now.saturating_add(config.interlude_length);
		let leadin_length = config.leadin_length;
		let ideal_cores_sold = (config.ideal_bulk_proportion * cores_offered as u32) as u16;
		let sellout_price = if cores_offered > 0 {
			// No core sold -> price was too high -> we have to adjust downwards.
			Some(new_prices.end_price)
		} else {
			None
		};

		let sale_index = old_sale.sale_index.saturating_add(1);

		// Update SaleInfo
		let new_sale = SaleInfoRecord {
			sale_start,
			leadin_length,
			end_price: new_prices.end_price,
			sellout_price,
			region_begin,
			region_end,
			first_core,
			ideal_cores_sold,
			cores_offered,
			cores_sold: 0,
			sale_index,
		};

		SaleInfo::<T>::put(&new_sale);

		Self::renew_cores(&new_sale);

		Self::deposit_event(Event::SaleInitialized {
			sale_start,
			leadin_length,
			start_price: Self::sale_price(&new_sale, now),
			end_price: new_prices.end_price,
			region_begin,
			region_end,
			ideal_cores_sold,
			cores_offered,
			sale_index,
		});

		Some(())
	}

	pub(crate) fn process_pool(when: Timeslice, status: &mut StatusRecord) {
		let pool_io = InstaPoolIo::<T>::take(when);
		status.private_pool_size = (status.private_pool_size as SignedCoreMaskBitCount)
			.saturating_add(pool_io.private) as CoreMaskBitCount;
		status.system_pool_size = (status.system_pool_size as SignedCoreMaskBitCount)
			.saturating_add(pool_io.system) as CoreMaskBitCount;
		let record = InstaPoolHistoryRecord {
			private_contributions: status.private_pool_size,
			system_contributions: status.system_pool_size,
			maybe_payout: None,
		};
		InstaPoolHistory::<T>::insert(when, record);
		Self::deposit_event(Event::<T>::HistoryInitialized {
			when,
			private_pool_size: status.private_pool_size,
			system_pool_size: status.system_pool_size,
		});
	}

	/// Schedule cores for the given `timeslice`.
	pub(crate) fn process_core_schedule(
		timeslice: Timeslice,
		rc_begin: RelayBlockNumberOf<T>,
		core: CoreIndex,
	) {
		let Some(workplan) = Workplan::<T>::take((timeslice, core)) else { return };
		let workload = Workload::<T>::get(core);
		let parts_used = workplan.iter().map(|i| i.mask).fold(CoreMask::void(), |a, i| a | i);
		let mut workplan = workplan.into_inner();
		workplan.extend(workload.into_iter().filter(|i| (i.mask & parts_used).is_void()));
		let workplan = Schedule::truncate_from(workplan);
		Workload::<T>::insert(core, &workplan);

		let mut total_used = 0;
		let mut intermediate = workplan
			.into_iter()
			.map(|i| (i.assignment, i.mask.count_ones() as u16 * (57_600 / 80)))
			.inspect(|i| total_used.saturating_accrue(i.1))
			.collect::<Vec<_>>();
		if total_used < 57_600 {
			intermediate.push((CoreAssignment::Idle, 57_600 - total_used));
		}
		intermediate.sort();
		let mut assignment: Vec<(CoreAssignment, PartsOf57600)> =
			Vec::with_capacity(intermediate.len());
		for i in intermediate.into_iter() {
			if let Some(ref mut last) = assignment.last_mut() {
				if last.0 == i.0 {
					last.1 += i.1;
					continue;
				}
			}
			assignment.push(i);
		}
		T::Coretime::assign_core(core, rc_begin, assignment.clone(), None);
		Self::deposit_event(Event::<T>::CoreAssigned { core, when: rc_begin, assignment });
	}

	/// Renews all the cores which have auto-renewal enabled.
	pub(crate) fn renew_cores(sale: &SaleInfoRecordOf<T>) {
		let renewals = AutoRenewals::<T>::get();

		let Ok(auto_renewals) = renewals
			.into_iter()
			.flat_map(|record| {
				// Check if the next renewal is scheduled further in the future than the start of
				// the next region beginning. If so, we skip the renewal for this core.
				if sale.region_begin < record.next_renewal {
					return Some(record);
				}

				let Some(payer) = T::SovereignAccountOf::maybe_convert(record.task) else {
					Self::deposit_event(Event::<T>::AutoRenewalFailed {
						core: record.core,
						payer: None,
					});
					return None;
				};

				// A core index can be assigned a different workload each bulk period, so this
				// is not necessarily the workload that was validated when auto-renewal was
				// enabled. The task must only pay for renewing its own workload.
				let renewal_id = PotentialRenewalId { core: record.core, when: sale.region_begin };
				let renews_own_workload = PotentialRenewals::<T>::get(renewal_id)
					.map_or(false, |renewal| {
						renewal.completion.is_complete_and_contains_task(record.task)
					});
				if !renews_own_workload {
					Self::deposit_event(Event::<T>::AutoRenewalFailed {
						core: record.core,
						payer: Some(payer),
					});
					return None;
				}

				if let Ok(new_core_index) = Self::do_renew(payer.clone(), record.core) {
					Some(AutoRenewalRecord {
						core: new_core_index,
						task: record.task,
						next_renewal: sale.region_end,
					})
				} else {
					Self::deposit_event(Event::<T>::AutoRenewalFailed {
						core: record.core,
						payer: Some(payer),
					});

					None
				}
			})
			.collect::<Vec<AutoRenewalRecord>>()
			.try_into()
		else {
			Self::deposit_event(Event::<T>::AutoRenewalLimitReached);
			return;
		};

		AutoRenewals::<T>::set(auto_renewals);
	}
}
