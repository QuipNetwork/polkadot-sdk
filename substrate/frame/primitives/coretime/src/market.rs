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

//! Generic coretime market interface.
//!
//! Contains the [`Market`] trait — an abstraction that allows `pallet-broker` to work with any
//! market logic implementing [`Market`].

use alloc::vec::Vec;
use core::fmt::Debug;

use codec::{Codec, DecodeWithMemTracking, EncodeLike, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_runtime::DispatchError;
use sp_weights::WeightMeter;

use crate::{CoreIndex, PotentialRenewalId, RegionId, Timeslice};

/// Trait representing generic coretime market logic.
///
/// ## Assumptions about the market implementations
/// - There are two types of orders: *purchase* and *renewal*.
/// - Every successful order either creates a bid or is resolved immediately.
/// - Coretime regions are equivalent from the user's perspective.
///
/// ## Market lifecycle
/// 1. [`Market::start_sales`] — initializes the market (if required).
/// 2. [`Market::place_order`], [`Market::place_renewal_order`], and [`Market::adjust_bid`] — users
///    purchase or bid for coretime regions and renew existing ones.
/// 3. [`Market::tick`] — called from `on_initialize` hook to execute time-dependent logic.
pub trait Market<RelayBlockNumber, Balance, AccountId> {
	/// Error type returned by market operations.
	type Error: Into<DispatchError>;

	/// Unique identifier assigned to each bid.
	type BidId: Copy + Debug + Codec + MaxEncodedLen + TypeInfo + Eq;

	/// Initialization data used in [`Market::start_sales`].
	type InitData: Codec + DecodeWithMemTracking + EncodeLike + Clone + Eq + Debug + TypeInfo;

	/// Configuration of the market.
	///
	/// Can be set in the [`Market::configure`].
	type Configuration: Codec + DecodeWithMemTracking + EncodeLike + Clone + Eq + Debug + TypeInfo;

	/// Provides information about available cores.
	type CoreRangeProvider: CoreRangeProvider;

	/// Provides information about timeslice scheduling.
	type TimesliceProvider: TimesliceProvider;

	/// Set or update the market configuration.
	///
	/// ### Parameters
	/// - `configuration`: a new configuration to use.
	fn configure(configuration: Self::Configuration) -> Result<(), Self::Error>;

	/// Start the coretime sales.
	///
	/// ### Parameters
	/// - `block_number`: Current relay chain block number.
	/// - `init_data`: Market-specific initialization data.
	fn start_sales(
		block_number: RelayBlockNumber,
		init_data: Self::InitData,
	) -> Result<SalesStarted<RelayBlockNumber>, Self::Error>;

	/// Place an order to purchase one coretime region.
	///
	/// Depending on the implementation, this either: places a bid, or immediately executes the
	/// purchase.
	///
	/// ### Parameters
	/// - `block_number`: Current relay chain block number.
	/// - `who`: Account placing the order.
	/// - `price_limit`: Maximum price the buyer is willing to pay.
	fn place_order(
		block_number: RelayBlockNumber,
		who: &AccountId,
		price_limit: Balance,
	) -> Result<OrderResult<Balance, Self::BidId>, Self::Error>;

	/// Place an order to renew a coretime region.
	///
	/// Depending on the implementation, this either: places a bid, or immediately executes the
	/// purchase.
	///
	/// ### Parameters
	/// - `block_number`: Current relay chain block number.
	/// - `who`: Account placing the order.
	/// - `renewal`: Renewal identifier.
	fn place_renewal_order(
		block_number: RelayBlockNumber,
		who: &AccountId,
		renewal: PotentialRenewalId,
	) -> Result<RenewalOrderResult<Balance, Self::BidId>, Self::Error>;

	/// Adjust the price of an existing bid.
	///
	/// This call may fail if the market does not allow increasing, decreasing,
	/// or withdrawing bids.
	///
	/// ### Parameters
	/// - `block_number`: Current relay chain block number.
	/// - `id`: The identifier of the bid to adjust.
	/// - `who`: Account adjusting the bid.
	/// - `new_price`: The new bid price. If `None` is provided, the bid will be withdrawn.
	fn adjust_bid(
		block_number: RelayBlockNumber,
		id: Self::BidId,
		who: &AccountId,
		new_price: Option<Balance>,
	) -> Result<AdjustBidResult<Balance>, Self::Error>;

	/// Execute time-based market logic.
	///
	/// This function is called from the `on_initialize` hook by `pallet-broker`.
	///
	/// ### Parameters
	/// - `now`: Current relay chain block number.
	/// - `weight_meter`: Used for advanced weight accounting.
	fn tick(
		now: RelayBlockNumber,
		weight_meter: &mut WeightMeter,
	) -> Vec<TickAction<AccountId, Balance, RelayBlockNumber>>;
}

/// Provides information about the range of cores that can be sold on a market.
pub trait CoreRangeProvider {
	/// Returns the range of core indices that can be sold on a market.
	///
	/// Returns `None` if the range is unknown (e.g., the [`CoreRangeProvider`]
	/// implementer is not initialized).
	fn core_range() -> Option<SoldCoresRange>;
}

/// A range of cores available for sale on a coretime market.
///
/// Represents the half-open range `[from, to)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SoldCoresRange {
	/// Minimum core index (inclusive).
	pub from: CoreIndex,
	/// Maximum core index (exclusive).
	pub to: CoreIndex,
}

/// Provides timeslice-related information to the market implementation.
pub trait TimesliceProvider {
	/// Returns the next timeslice pending commitment, if any.
	fn next_timeslice_to_commit() -> Option<Timeslice>;
	/// Returns the latest timeslice ready to be committed to the relay chain.
	///
	/// Returns `None` if the timeslice is unknown (e.g., when [`TimesliceProvider`] implementer is
	/// not yet initialized).
	fn latest_timeslice_ready_to_commit() -> Option<Timeslice>;
}

/// Information about the sale.
pub struct MarketSaleInfo<RelayBlockNumber> {
	/// The relay block number at which the sale will/did start.
	pub sale_start: RelayBlockNumber,
	/// The first timeslice of the Regions which are being sold in this sale.
	pub region_begin: Timeslice,
	/// The timeslice on which the Regions which are being sold in the sale terminate. (i.e. One
	/// after the last timeslice which the Regions control.)
	pub region_end: Timeslice,
	/// Number of cores which are/have been offered for sale.
	pub cores_offered: CoreIndex,
	/// The index of the first core which is for sale. Core of Regions which are sold have
	/// incrementing indices from this.
	pub first_core: CoreIndex,
	/// Number of cores which have been sold; never more than cores_offered.
	pub cores_sold: CoreIndex,
}

/// Outcome of [`Market::start_sales`].
pub struct SalesStarted<RelayBlockNumber> {
	/// The first active sale.
	pub sale: MarketSaleInfo<RelayBlockNumber>,
}

/// Possible outcomes of [`Market::place_order`].
pub enum OrderResult<Balance, BidId> {
	/// A bid was placed.
	BidPlaced {
		/// Identifier of the bid.
		id: BidId,
		/// Amount to lock when placing the bid.
		bid_price: Balance,
	},
	/// The region was purchased immediately.
	Sold {
		/// Price paid.
		price: Balance,
		/// Purchased region identifier.
		region_id: RegionId,
		/// End of the purchased region.
		region_end: Timeslice,
	},
}

/// Possible outcomes of [`Market::place_renewal_order`].
pub enum RenewalOrderResult<Balance, BidId> {
	/// A renewal bid was placed.
	BidPlaced {
		/// Identifier of the bid.
		id: BidId,
		/// Amount to lock when placing the bid.
		bid_price: Balance,
	},
	/// The region was renewed immediately.
	Renewed {
		/// Price paid for the renewal.
		price: Balance,
		/// Identifier of the renewed region.
		region_id: RegionId,
		/// End of the renewed region.
		effective_to: Timeslice,
	},
}

/// Outcome of [`Market::adjust_bid`].
pub enum AdjustBidResult<Balance> {
	/// Indicates that additional balance must be locked.
	Lock {
		/// The additional amount to lock.
		amount: Balance,
	},
	/// Indicates that part or all of the bid should be refunded.
	Refund {
		/// The amount to refund.
		amount: Balance,
	},
}

/// Outcome of [`Market::tick`].
///
/// When `pallet-broker` calls [`Market::tick`], it receives a list of [`TickAction`]s
/// which will be executed in order.
///
/// These actions are **not** executed by the market itself as they don't fall into the scope of the
/// market logic(e.g., transferring balances, updating region ownership). Instead, the market
/// relies on `pallet-broker` to execute them.
pub enum TickAction<AccountId, Balance, RelayBlockNumber> {
	/// Sell a region to an account.
	SellRegion {
		/// New owner.
		owner: AccountId,
		/// Total price paid.
		paid: Balance,
		/// Region identifier.
		region_id: RegionId,
		/// End of the region.
		region_end: Timeslice,
	},
	/// Renew an existing region.
	RenewRegion {
		/// Current owner.
		owner: AccountId,
		/// Renewal identifier.
		renewal_id: PotentialRenewalId,
	},
	/// Refund previously locked balance.
	Refund {
		/// Amount to return.
		amount: Balance,
		/// Recipient.
		who: AccountId,
	},
	/// Process the auto renewals which are stored in `pallet-broker`.
	ProcessAutoRenewals {
		/// Only auto-renewals allowing renewals after this timeslice should be processed.
		after_timeslice: Timeslice,
		/// When the next auto-renewal of this core can be made.
		next_renewal_at: Timeslice,
	},
	/// Indicates that a new sale cycle has started.
	///
	/// This allows `pallet-broker` to handle sale boundary transitions.
	SaleRotated {
		/// Previously active sale.
		old_sale: MarketSaleInfo<RelayBlockNumber>,
		/// Newly active sale.
		new_sale: MarketSaleInfo<RelayBlockNumber>,
	},
}
