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

//! The parachain on demand assignment module.
//!
//! Implements a mechanism for taking in orders for on-demand parachain (previously parathreads)
//! assignments. This module is not handled by the initializer but is instead instantiated in the
//! `construct_runtime` macro.
//!
//! The module uses a single queue for all on-demand orders. Orders are generally processed in the
//! order they are received, but with an important constraint: only one order per ParaId can be
//! assigned in each scheduling round. If multiple orders for the same ParaId exist in the queue,
//! only the first will be assigned, and subsequent orders for that ParaId will remain queued until
//! the next round.

use core::mem;

use sp_runtime::traits::Zero;
mod benchmarking;
pub mod migration;

extern crate alloc;

use crate::{configuration, paras};
use alloc::{collections::BTreeSet, vec::Vec};
use frame_support::{
	pallet_prelude::*,
	traits::{
		defensive_prelude::*,
		Currency,
		ExistenceRequirement::{self, AllowDeath, KeepAlive},
		WithdrawReasons,
	},
	PalletId,
};
use frame_system::{pallet_prelude::*, Pallet as System};
use polkadot_primitives::{Id as ParaId, ON_DEMAND_MAX_QUEUE_MAX_SIZE};
use sp_runtime::{
	traits::{AccountIdConversion, One, SaturatedConversion},
	FixedPointNumber, FixedPointOperand, FixedU128, Perbill, Saturating,
};

pub use pallet::*;

mod mock_helpers;
#[cfg(test)]
mod tests;

const LOG_TARGET: &str = "runtime::parachains::on-demand";

pub trait WeightInfo {
	fn place_order_allow_death() -> Weight;
	fn place_order_keep_alive() -> Weight;
	fn place_order_with_credits() -> Weight;
}

/// A weight info that is only suitable for testing.
pub struct TestWeightInfo;

impl WeightInfo for TestWeightInfo {
	fn place_order_allow_death() -> Weight {
		Weight::MAX
	}

	fn place_order_keep_alive() -> Weight {
		Weight::MAX
	}

	fn place_order_with_credits() -> Weight {
		Weight::MAX
	}
}

/// Defines how the account wants to pay for on-demand.
#[derive(Encode, Decode, TypeInfo, Debug, PartialEq, Clone, Eq)]
enum PaymentType {
	/// Use credits to purchase on-demand coretime.
	Credits,
	/// Use account's free balance to purchase on-demand coretime.
	Balance,
}

/// Shorthand for the Balance type the runtime is using.
pub type BalanceOf<T> =
	<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// All queued on-demand orders.
#[derive(Encode, Decode, TypeInfo)]
pub struct OrderQueue<N> {
	queue: BoundedVec<EnqueuedOrder<N>, ConstU32<ON_DEMAND_MAX_QUEUE_MAX_SIZE>>,
}

impl<N> OrderQueue<N> {
	/// Pop `num_cores` from the queue, assuming `now` as the current block number.
	pub fn pop_assignment_for_cores<T: Config>(
		&mut self,
		now: N,
		mut num_cores: u32,
	) -> impl Iterator<Item = ParaId>
	where
		N: Saturating + Ord + One + Copy,
	{
		let mut popped = BTreeSet::new();
		let mut remaining_orders = Vec::with_capacity(self.queue.len());
		for order in mem::take(&mut self.queue) {
			// Order is ready 2 blocks later (asynchronous backing):
			let ready_at = order.ordered_at.saturating_plus_one().saturating_plus_one();
			let is_ready = ready_at <= now;

			if num_cores > 0 && is_ready && popped.insert(order.para_id) {
				num_cores -= 1;
			} else {
				remaining_orders.push(order);
			}
		}
		self.queue = BoundedVec::truncate_from(remaining_orders);
		popped.into_iter()
	}

	fn new() -> Self {
		OrderQueue { queue: BoundedVec::new() }
	}

	/// Try to push an additional order.
	///
	/// Fails if queue is already at capacity.
	fn try_push(&mut self, now: N, para_id: ParaId) -> Result<(), ParaId> {
		self.queue
			.try_push(EnqueuedOrder { para_id, ordered_at: now })
			.map_err(|o| o.para_id)
	}

	fn len(&self) -> usize {
		self.queue.len()
	}
}

/// Data about a placed on-demand order.
#[derive(Encode, Decode, TypeInfo)]
struct EnqueuedOrder<N> {
	/// The parachain the order was placed for.
	para_id: ParaId,
	/// The block number the order came in.
	ordered_at: N,
}

/// Queue data for on-demand.
#[derive(Encode, Decode, TypeInfo)]
struct OrderStatus<N> {
	/// Last calculated traffic value.
	traffic: FixedU128,

	/// Enqueued orders.
	queue: OrderQueue<N>,
}

impl<N> Default for OrderStatus<N> {
	fn default() -> OrderStatus<N> {
		OrderStatus { traffic: FixedU128::default(), queue: OrderQueue::new() }
	}
}

/// Errors that can happen during spot traffic calculation.
#[derive(PartialEq, Debug)]
pub enum SpotTrafficCalculationErr {
	/// The order queue capacity is at 0.
	QueueCapacityIsZero,
	/// The queue size is larger than the queue capacity.
	QueueSizeLargerThanCapacity,
	/// Arithmetic error during division, either division by 0 or over/underflow.
	Division,
}

#[frame_support::pallet]
pub mod pallet {

	use super::*;
	use polkadot_primitives::Id as ParaId;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(2);

	#[pallet::pallet]
	#[pallet::without_storage_info]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + configuration::Config + paras::Config {
		/// The runtime's definition of an event.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The runtime's definition of a Currency.
		type Currency: Currency<Self::AccountId>;

		/// Something that provides the weight of this pallet.
		type WeightInfo: WeightInfo;

		/// The default value for the spot traffic multiplier.
		#[pallet::constant]
		type TrafficDefaultValue: Get<FixedU128>;

		/// The maximum number of blocks some historical revenue
		/// information stored for.
		#[pallet::constant]
		type MaxHistoricalRevenue: Get<u32>;

		/// Identifier for the internal revenue balance.
		#[pallet::constant]
		type PalletId: Get<PalletId>;
	}

	/// Priority queue for all orders which don't yet (or not any more) have any core affinity.
	#[pallet::storage]
	pub(super) type OrderStatus<T: Config> =
		StorageValue<_, super::OrderStatus<BlockNumberFor<T>>, ValueQuery>;

	/// Keeps track of accumulated revenue from on demand order sales.
	#[pallet::storage]
	pub(super) type Revenue<T: Config> =
		StorageValue<_, BoundedVec<BalanceOf<T>, T::MaxHistoricalRevenue>, ValueQuery>;

	/// Keeps track of credits owned by each account.
	#[pallet::storage]
	pub type Credits<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AccountId, BalanceOf<T>, ValueQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// An order was placed at some spot price amount by orderer ordered_by
		OnDemandOrderPlaced { para_id: ParaId, spot_price: BalanceOf<T>, ordered_by: T::AccountId },
		/// The value of the spot price has likely changed
		SpotPriceSet { spot_price: BalanceOf<T> },
		/// An account was given credits.
		AccountCredited { who: T::AccountId, amount: BalanceOf<T> },
		/// The order queue was unexpectedly full - this indicates a problem with the configuration
		/// of the on-demand pallet on the Coretime chain. `dropped` indicates how many orders were
		/// dropped because they couldn't be queued.
		UnexpectedQueueFull { dropped: u32 },
		/// A batch of on-demand orders from the broker chain has been queued.
		BatchQueued { batch: Vec<(ParaId, BlockNumberFor<T>)> },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The order queue is full, `place_order` will not continue.
		QueueFull,
		/// The current spot price is higher than the max amount specified in the `place_order`
		/// call, making it invalid.
		SpotPriceHigherThanMaxAmount,
		/// The account doesn't have enough credits to purchase on-demand coretime.
		InsufficientCredits,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_now: BlockNumberFor<T>) -> Weight {
			// Update revenue information storage.
			Revenue::<T>::mutate(|revenue| {
				if let Some(overdue) =
					revenue.force_insert_keep_left(0, 0u32.into()).defensive_unwrap_or(None)
				{
					// We have some overdue revenue not claimed by the Coretime Chain, let's
					// accumulate it at the oldest stored block
					if let Some(last) = revenue.last_mut() {
						*last = last.saturating_add(overdue);
					}
				}
			});

			let config = configuration::ActiveConfig::<T>::get();
			// We need to update the spot traffic on block initialize in order to account for idle
			// blocks.
			OrderStatus::<T>::mutate(|order_status| {
				Self::update_spot_traffic(&config, order_status);
			});

			// Reads: `Revenue`, `ActiveConfig`, `OrderStatus`
			// Writes: `Revenue`, `OrderStatus`
			T::DbWeight::get().reads_writes(3, 2)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Create a single on demand core order.
		/// Will use the spot price for the current block and will reap the account if needed.
		///
		/// Parameters:
		/// - `origin`: The sender of the call, funds will be withdrawn from this account.
		/// - `max_amount`: The maximum balance to withdraw from the origin to place an order.
		/// - `para_id`: A `ParaId` the origin wants to provide blockspace for.
		///
		/// Errors:
		/// - `InsufficientBalance`: from the Currency implementation
		/// - `QueueFull`
		/// - `SpotPriceHigherThanMaxAmount`
		///
		/// Events:
		/// - `OnDemandOrderPlaced`
		#[pallet::call_index(0)]
		#[pallet::weight(<T as Config>::WeightInfo::place_order_allow_death())]
		#[allow(deprecated)]
		#[deprecated(note = "This will be removed in favor of using `place_order_with_credits`")]
		pub fn place_order_allow_death(
			origin: OriginFor<T>,
			max_amount: BalanceOf<T>,
			para_id: ParaId,
		) -> DispatchResult {
			let sender = ensure_signed(origin)?;
			Pallet::<T>::do_place_order(
				sender,
				max_amount,
				para_id,
				AllowDeath,
				PaymentType::Balance,
			)
		}

		/// Same as the [`place_order_allow_death`](Self::place_order_allow_death) call , but with a
		/// check that placing the order will not reap the account.
		///
		/// Parameters:
		/// - `origin`: The sender of the call, funds will be withdrawn from this account.
		/// - `max_amount`: The maximum balance to withdraw from the origin to place an order.
		/// - `para_id`: A `ParaId` the origin wants to provide blockspace for.
		///
		/// Errors:
		/// - `InsufficientBalance`: from the Currency implementation
		/// - `QueueFull`
		/// - `SpotPriceHigherThanMaxAmount`
		///
		/// Events:
		/// - `OnDemandOrderPlaced`
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config>::WeightInfo::place_order_keep_alive())]
		#[allow(deprecated)]
		#[deprecated(note = "This will be removed in favor of using `place_order_with_credits`")]
		pub fn place_order_keep_alive(
			origin: OriginFor<T>,
			max_amount: BalanceOf<T>,
			para_id: ParaId,
		) -> DispatchResult {
			let sender = ensure_signed(origin)?;
			Pallet::<T>::do_place_order(
				sender,
				max_amount,
				para_id,
				KeepAlive,
				PaymentType::Balance,
			)
		}

		/// Create a single on demand core order with credits.
		/// Will charge the owner's on-demand credit account the spot price for the current block.
		///
		/// Parameters:
		/// - `origin`: The sender of the call, on-demand credits will be withdrawn from this
		///   account.
		/// - `max_amount`: The maximum number of credits to spend from the origin to place an
		///   order.
		/// - `para_id`: A `ParaId` the origin wants to provide blockspace for.
		///
		/// Errors:
		/// - `InsufficientCredits`
		/// - `QueueFull`
		/// - `SpotPriceHigherThanMaxAmount`
		///
		/// Events:
		/// - `OnDemandOrderPlaced`
		#[pallet::call_index(2)]
		#[pallet::weight(<T as Config>::WeightInfo::place_order_with_credits())]
		pub fn place_order_with_credits(
			origin: OriginFor<T>,
			max_amount: BalanceOf<T>,
			para_id: ParaId,
		) -> DispatchResult {
			let sender = ensure_signed(origin)?;
			Pallet::<T>::do_place_order(
				sender,
				max_amount,
				para_id,
				KeepAlive,
				PaymentType::Credits,
			)
		}
	}
}

// Internal functions and interface to scheduler/wrapping assignment provider.
impl<T: Config> Pallet<T>
where
	BalanceOf<T>: FixedPointOperand,
{
	/// Pop assignments for the given number of on-demand cores in a block.
	pub fn pop_assignment_for_cores(
		now: BlockNumberFor<T>,
		num_cores: u32,
	) -> impl Iterator<Item = ParaId> {
		pallet::OrderStatus::<T>::mutate(|order_status| {
			order_status.queue.pop_assignment_for_cores::<T>(now, num_cores)
		})
	}

	/// Look into upcoming orders.
	///
	/// The returned `OrderQueue` allows for simulating upcoming
	/// `pop_assignment_for_cores` calls.
	///
	/// **Note**: The current implementation returns the entire queue (up to 10,000 orders).
	/// Callers typically only need `num_cores * scheduling_lookahead` orders (e.g., 10 cores *
	/// 5 lookahead = 50 orders). Future implementations should consider adding a limit parameter
	/// to avoid returning unnecessary data and enable more efficient storage schemes.
	pub fn peek_order_queue() -> OrderQueue<BlockNumberFor<T>> {
		pallet::OrderStatus::<T>::get().queue
	}

	/// Push an order back to the back of the queue.
	///
	/// The order could not be served for some reason, give it another chance.
	///
	/// Parameters:
	/// - `para_id`: The para that did not make it.
	pub fn push_back_order(para_id: ParaId) {
		pallet::OrderStatus::<T>::mutate(|order_status| {
			let now = <frame_system::Pallet<T>>::block_number();
			if let Err(e) = order_status.queue.try_push(now, para_id) {
				log::debug!(target: LOG_TARGET, "Pushing back order failed (queue too long): {:?}", e);
			};
		});
	}

	/// Adds credits to the specified account.
	///
	/// Parameters:
	/// - `who`: Credit receiver.
	/// - `amount`: The amount of new credits the account will receive.
	pub fn credit_account(who: T::AccountId, amount: BalanceOf<T>) {
		Credits::<T>::mutate(who.clone(), |credits| {
			*credits = credits.saturating_add(amount);
		});
		Pallet::<T>::deposit_event(Event::<T>::AccountCredited { who, amount });
	}

	/// Helper function for `place_order_*` calls. Used to differentiate between placing orders
	/// with a keep alive check or to allow the account to be reaped. The amount charged is
	/// stored to the pallet account to be later paid out as revenue.
	///
	/// Parameters:
	/// - `sender`: The sender of the call, funds will be withdrawn from this account.
	/// - `max_amount`: The maximum balance to withdraw from the origin to place an order.
	/// - `para_id`: A `ParaId` the origin wants to provide blockspace for.
	/// - `existence_requirement`: Whether or not to ensure that the account will not be reaped.
	/// - `payment_type`: Defines how the user wants to pay for on-demand.
	///
	/// Errors:
	/// - `InsufficientBalance`: from the Currency implementation
	/// - `QueueFull`
	/// - `SpotPriceHigherThanMaxAmount`
	///
	/// Events:
	/// - `OnDemandOrderPlaced`
	fn do_place_order(
		sender: <T as frame_system::Config>::AccountId,
		max_amount: BalanceOf<T>,
		para_id: ParaId,
		existence_requirement: ExistenceRequirement,
		payment_type: PaymentType,
	) -> DispatchResult {
		let config = configuration::ActiveConfig::<T>::get();

		pallet::OrderStatus::<T>::mutate(|order_status| {
			Self::update_spot_traffic(&config, order_status);
			let traffic = order_status.traffic;

			// Calculate spot price
			let spot_price: BalanceOf<T> = traffic.saturating_mul_int(
				config.scheduler_params.on_demand_base_fee.saturated_into::<BalanceOf<T>>(),
			);

			// Is the current price higher than `max_amount`
			ensure!(spot_price.le(&max_amount), Error::<T>::SpotPriceHigherThanMaxAmount);

			ensure!(
				order_status.queue.len() <
					config.scheduler_params.on_demand_queue_max_size as usize,
				Error::<T>::QueueFull
			);

			match payment_type {
				PaymentType::Balance => {
					// Charge the sending account the spot price. The amount will be teleported to
					// the broker chain once it requests revenue information.
					let amt = T::Currency::withdraw(
						&sender,
						spot_price,
						WithdrawReasons::FEE,
						existence_requirement,
					)?;

					// Consume the negative imbalance and deposit it into the pallet account. Make
					// sure the account preserves even without the existential deposit.
					let pot = Self::account_id();
					if !System::<T>::account_exists(&pot) {
						System::<T>::inc_providers(&pot);
					}
					T::Currency::resolve_creating(&pot, amt);
				},
				PaymentType::Credits => {
					let credits = Credits::<T>::get(&sender);

					// Charge the sending account the spot price in credits.
					let new_credits_value =
						credits.checked_sub(&spot_price).ok_or(Error::<T>::InsufficientCredits)?;

					if new_credits_value.is_zero() {
						Credits::<T>::remove(&sender);
					} else {
						Credits::<T>::insert(&sender, new_credits_value);
					}
				},
			}

			// Add the amount to the current block's (index 0) revenue information.
			Revenue::<T>::mutate(|bounded_revenue| {
				if let Some(current_block) = bounded_revenue.get_mut(0) {
					*current_block = current_block.saturating_add(spot_price);
				} else {
					// Revenue has already been claimed in the same block, including the block
					// itself. It shouldn't normally happen as revenue claims in the future are
					// not allowed.
					bounded_revenue.try_push(spot_price).defensive_ok();
				}
			});

			let now = <frame_system::Pallet<T>>::block_number();
			order_status
				.queue
				.try_push(now, para_id)
				.defensive_map_err(|_| Error::<T>::QueueFull)?;

			Pallet::<T>::deposit_event(Event::<T>::OnDemandOrderPlaced {
				para_id,
				spot_price,
				ordered_by: sender,
			});

			Ok(())
		})
	}

	/// Adds a batch of coretime orders to the queue.
	pub fn queue_order_batch(batch: &[(ParaId, BlockNumberFor<T>)]) {
		pallet::OrderStatus::<T>::mutate(|order_status| {
			// The number of successfully queued orders happens to be the same as the index of the
			// order being currently processed.
			for (queued, (para_id, ordered_at)) in batch.iter().enumerate() {
				if let Err(err) = order_status.queue.try_push(*ordered_at, *para_id) {
					log::debug!(
						target: LOG_TARGET,
						"Error trying to push an order to the queue: {:?}", err
					);
					Pallet::<T>::deposit_event(Event::<T>::UnexpectedQueueFull {
						dropped: (batch.len() - queued) as u32,
					});
					return;
				}
			}
			Pallet::<T>::deposit_event(Event::<T>::BatchQueued { batch: batch.to_vec() });
		});
	}

	/// Calculate and update spot traffic.
	fn update_spot_traffic(
		config: &configuration::HostConfiguration<BlockNumberFor<T>>,
		order_status: &mut OrderStatus<BlockNumberFor<T>>,
	) {
		let old_traffic = order_status.traffic;
		match Self::calculate_spot_traffic(
			old_traffic,
			config.scheduler_params.on_demand_queue_max_size,
			order_status.queue.len() as u32,
			config.scheduler_params.on_demand_target_queue_utilization,
			config.scheduler_params.on_demand_fee_variability,
		) {
			Ok(new_traffic) => {
				// Only update storage on change
				if new_traffic != old_traffic {
					order_status.traffic = new_traffic;

					// calculate the new spot price
					let spot_price: BalanceOf<T> = new_traffic.saturating_mul_int(
						config.scheduler_params.on_demand_base_fee.saturated_into::<BalanceOf<T>>(),
					);

					// emit the event for updated new price
					Pallet::<T>::deposit_event(Event::<T>::SpotPriceSet { spot_price });
				}
			},
			Err(err) => {
				log::debug!(
					target: LOG_TARGET,
					"Error calculating spot traffic: {:?}", err
				);
			},
		};
	}

	/// The spot price multiplier. This is based on the transaction fee calculations defined in:
	/// https://research.web3.foundation/Polkadot/overview/token-economics#setting-transaction-fees
	///
	/// Parameters:
	/// - `traffic`: The previously calculated multiplier, can never go below 1.0.
	/// - `queue_capacity`: The max size of the order book.
	/// - `queue_size`: How many orders are currently in the order book.
	/// - `target_queue_utilisation`: How much of the queue_capacity should be ideally occupied,
	///   expressed in percentages(perbill).
	/// - `variability`: A variability factor, i.e. how quickly the spot price adjusts. This number
	///   can be chosen by p/(k*(1-s)) where p is the desired ratio increase in spot price over k
	///   number of blocks. s is the target_queue_utilisation. A concrete example: v =
	///   0.05/(20*(1-0.25)) = 0.0033.
	///
	/// Returns:
	/// - A `FixedU128` in the range of  `Config::TrafficDefaultValue` - `FixedU128::MAX` on
	///   success.
	///
	/// Errors:
	/// - `SpotTrafficCalculationErr::QueueCapacityIsZero`
	/// - `SpotTrafficCalculationErr::QueueSizeLargerThanCapacity`
	/// - `SpotTrafficCalculationErr::Division`
	fn calculate_spot_traffic(
		traffic: FixedU128,
		queue_capacity: u32,
		queue_size: u32,
		target_queue_utilisation: Perbill,
		variability: Perbill,
	) -> Result<FixedU128, SpotTrafficCalculationErr> {
		// Return early if queue has no capacity.
		if queue_capacity == 0 {
			return Err(SpotTrafficCalculationErr::QueueCapacityIsZero);
		}

		// Return early if queue size is greater than capacity.
		if queue_size > queue_capacity {
			return Err(SpotTrafficCalculationErr::QueueSizeLargerThanCapacity);
		}

		// (queue_size / queue_capacity) - target_queue_utilisation
		let queue_util_ratio = FixedU128::from_rational(queue_size.into(), queue_capacity.into());
		let positive = queue_util_ratio >= target_queue_utilisation.into();
		let queue_util_diff = queue_util_ratio.max(target_queue_utilisation.into()) -
			queue_util_ratio.min(target_queue_utilisation.into());

		// variability * queue_util_diff
		let var_times_qud = queue_util_diff.saturating_mul(variability.into());

		// variability^2 * queue_util_diff^2
		let var_times_qud_pow = var_times_qud.saturating_mul(var_times_qud);

		// (variability^2 * queue_util_diff^2)/2
		let div_by_two: FixedU128;
		match var_times_qud_pow.const_checked_div(2.into()) {
			Some(dbt) => div_by_two = dbt,
			None => return Err(SpotTrafficCalculationErr::Division),
		}

		// traffic * (1 + queue_util_diff) + div_by_two
		if positive {
			let new_traffic = queue_util_diff
				.saturating_add(div_by_two)
				.saturating_add(One::one())
				.saturating_mul(traffic);
			Ok(new_traffic.max(<T as Config>::TrafficDefaultValue::get()))
		} else {
			let new_traffic = queue_util_diff.saturating_sub(div_by_two).saturating_mul(traffic);
			Ok(new_traffic.max(<T as Config>::TrafficDefaultValue::get()))
		}
	}

	/// Collect the revenue from the `when` blockheight
	pub fn claim_revenue_until(when: BlockNumberFor<T>) -> BalanceOf<T> {
		let now = <frame_system::Pallet<T>>::block_number();
		let mut amount: BalanceOf<T> = BalanceOf::<T>::zero();
		Revenue::<T>::mutate(|revenue| {
			while !revenue.is_empty() {
				let index = (revenue.len() - 1) as u32;
				if when > now.saturating_sub(index.into()) {
					amount = amount.saturating_add(revenue.pop().defensive_unwrap_or(0u32.into()));
				} else {
					break;
				}
			}
		});

		amount
	}

	/// Account of the pallet pot, where the funds from instantaneous coretime sale are accumulated.
	pub fn account_id() -> T::AccountId {
		T::PalletId::get().into_account_truncating()
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub fn populate_queue(para_id: ParaId, num: u32) {
		let now = <frame_system::Pallet<T>>::block_number();
		pallet::OrderStatus::<T>::mutate(|order_status| {
			for _ in 0..num {
				order_status.queue.try_push(now, para_id).unwrap();
			}
		});
	}

	#[cfg(feature = "runtime-benchmarks")]
	pub(crate) fn set_revenue(rev: BoundedVec<BalanceOf<T>, T::MaxHistoricalRevenue>) {
		Revenue::<T>::put(rev);
	}

	#[cfg(test)]
	fn set_order_status(new_status: OrderStatus<BlockNumberFor<T>>) {
		pallet::OrderStatus::<T>::set(new_status);
	}

	#[cfg(test)]
	fn get_order_status() -> OrderStatus<BlockNumberFor<T>> {
		pallet::OrderStatus::<T>::get()
	}

	#[cfg(test)]
	fn get_traffic_default_value() -> FixedU128 {
		<T as Config>::TrafficDefaultValue::get()
	}

	#[cfg(test)]
	fn get_revenue() -> Vec<BalanceOf<T>> {
		Revenue::<T>::get().to_vec()
	}
}
