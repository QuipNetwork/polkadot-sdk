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

//! Types used by this pallet.

use crate::{
	Config, Error, RelayBlockNumberOf, DEFAULT_BASE_FEE, DEFAULT_DRAIN_RATE_PER_BLOCK,
	DEFAULT_ORDER_CAP, DEFAULT_PRICE_STEP,
};
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use fp_coretime::TaskId;
use frame_support::{
	pallet_prelude::*,
	traits::{fungible::Inspect, tokens::Balance},
};
use frame_system::Config as SConfig;
use scale_info::TypeInfo;
use sp_arithmetic::{
	traits::{ensure_pow, One, Saturating},
	FixedPointNumber, FixedU128, Perbill,
};
use sp_runtime::DispatchError;

pub type BalanceOf<T> = <<T as Config>::Currency as Inspect<<T as SConfig>::AccountId>>::Balance;

/// The parameters used for pricing on-demand orders.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct PriceParameters<Balance> {
	/// The maximum number of outstanding orders beyond which we reject new orders.
	pub order_cap: u32,
	/// The number of outstanding orders assumed to be drained per Relay-chain block, per pool
	/// core.
	pub drain_rate_per_block: u32,
	/// The spot price increase per outstanding order in the queue.
	pub price_step: Perbill,
	/// The base spot price when the queue is empty.
	pub base_fee: Balance,
}

impl<T> Default for PriceParameters<T>
where
	T: From<u32>,
{
	fn default() -> Self {
		Self {
			order_cap: DEFAULT_ORDER_CAP,
			drain_rate_per_block: DEFAULT_DRAIN_RATE_PER_BLOCK,
			price_step: Perbill::from_percent(DEFAULT_PRICE_STEP),
			base_fee: T::from(DEFAULT_BASE_FEE),
		}
	}
}

impl<Balance> PriceParameters<Balance> {
	pub(crate) fn validate<T>(&self) -> Result<(), Error<T>>
	where
		T: Config,
		T::Currency: Inspect<<T as SConfig>::AccountId, Balance = Balance>,
		Balance: frame_support::traits::tokens::Balance,
	{
		ensure!(
			T::PricingProvider::spot_price(self, self.order_cap).is_ok(),
			Error::<T>::OrderPriceCanOverflow
		);
		Ok(())
	}
}

pub type PriceParametersOf<T> = PriceParameters<BalanceOf<T>>;

/// Data about a placed on-demand order.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct EnqueuedOrder<RelayBlockNumber> {
	/// The parachain the order was placed for.
	pub para_id: TaskId,
	/// The Relay-chain block number the order came in at.
	pub ordered_at: RelayBlockNumber,
}

/// The locally tracked, estimated state of the Relay chain's on-demand order queue.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct QueueTracker<RelayBlockNumber> {
	/// The current estimated number of outstanding orders.
	pub outstanding_orders: u32,
	/// When the queue state was last updated.
	pub last_updated: RelayBlockNumber,
}

pub type QueueTrackerOf<T> = QueueTracker<RelayBlockNumberOf<T>>;

/// Type for getting information about the on-demand pool capacity.
pub trait PoolCapacityProvider {
	/// Returns how many cores are dedicated to the on-demand pool.
	fn pool_cores() -> u32;
}

/// Type for determining the prices of on-demand orders
pub trait PricingProvider<T: Balance> {
	/// Returns the current spot price of an on-demand order.
	fn spot_price(price_config: &PriceParameters<T>, queue_depth: u32) -> Result<T, DispatchError>;
}

/// A type implementing the default algorithm for determining the order price.
pub struct DefaultPricingProvider;

impl<T: Balance> PricingProvider<T> for DefaultPricingProvider {
	fn spot_price(price_config: &PriceParameters<T>, queue_depth: u32) -> Result<T, DispatchError> {
		// Every order already outstanding raises the price by `price_step`.
		let price_adjustment = ensure_pow(
			FixedU128::one().saturating_add(FixedU128::from_perbill(price_config.price_step)),
			queue_depth as usize,
		)?;
		Ok(price_adjustment.saturating_mul_int(price_config.base_fee))
	}
}
