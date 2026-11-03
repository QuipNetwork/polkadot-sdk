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

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
extern crate core;

use alloc::{vec, vec::Vec};
use codec::{Decode, Encode};
use core::{fmt::Debug, marker::PhantomData};
use frame_support::{
	dispatch::GetDispatchInfo,
	ensure,
	traits::{Contains, ContainsPair, Defensive, Get, PalletsInfoAccess},
};
use sp_core::defer;
use sp_io::hashing::blake2_128;
use sp_weights::Weight;
use xcm::{
	latest::{prelude::*, AssetTransferFilter},
	RECURSION_LIMIT,
};

pub mod traits;
use traits::{
	validate_export, AssetExchange, AssetLock, CallDispatcher, ClaimAssets, ConvertOrigin,
	DropAssets, Enact, EventEmitter, ExportXcm, FeeManager, FeeReason, HandleHrmpChannelAccepted,
	HandleHrmpChannelClosing, HandleHrmpNewChannelOpenRequest, OnResponse, ProcessTransaction,
	Properties, ShouldExecute, TransactAsset, VersionChangeNotifier, WeightBounds, WeightTrader,
	XcmAssetTransfers,
};

pub use traits::RecordXcm;

mod assets;
pub use assets::AssetsInHolding;
mod config;
use crate::assets::BackupAssetsInHolding;
pub use config::Config;

pub mod test_helpers;
#[cfg(test)]
mod tests;

/// A struct to specify how fees are being paid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FeesMode {
	/// If true, then the fee assets are taken directly from the origin's on-chain account,
	/// otherwise the fee assets are taken from the holding register.
	///
	/// Defaults to false.
	pub jit_withdraw: bool,
}

environmental::environmental!(recursion_count: u8);

/// The XCM executor.
pub struct XcmExecutor<Config: config::Config> {
	holding: AssetsInHolding,
	holding_limit: usize,
	context: XcmContext,
	original_origin: Location,
	trader: Config::Trader,
	/// The most recent error result and instruction index into the fragment in which it occurred,
	/// if any.
	error: Option<(u32, XcmError)>,
	/// The surplus weight, defined as the amount by which `max_weight` is
	/// an over-estimate of the actual weight consumed. We do it this way to avoid needing the
	/// execution engine to keep track of all instructions' weights (it only needs to care about
	/// the weight of dynamically determined instructions such as `Transact`).
	total_surplus: Weight,
	total_refunded: Weight,
	error_handler: Xcm<Config::RuntimeCall>,
	error_handler_weight: Weight,
	appendix: Xcm<Config::RuntimeCall>,
	appendix_weight: Weight,
	transact_status: MaybeErrorCode,
	fees_mode: FeesMode,
	fees: AssetsInHolding,
	/// Asset provided in last `BuyExecution` instruction (if any) in current XCM program. Same
	/// asset type will be used for paying any potential delivery fees incurred by the program.
	asset_used_in_buy_execution: Option<AssetId>,
	/// Stores the current message's weight.
	message_weight: Weight,
	asset_claimer: Option<Location>,
	already_paid_fees: bool,
	_config: PhantomData<Config>,
}

#[cfg(any(test, feature = "runtime-benchmarks"))]
impl<Config: config::Config> XcmExecutor<Config> {
	pub fn holding(&self) -> &AssetsInHolding {
		&self.holding
	}
	pub fn set_holding(&mut self, v: AssetsInHolding) {
		self.holding = v
	}
	pub fn holding_limit(&self) -> &usize {
		&self.holding_limit
	}
	pub fn set_holding_limit(&mut self, v: usize) {
		self.holding_limit = v
	}
	pub fn origin(&self) -> &Option<Location> {
		&self.context.origin
	}
	pub fn set_origin(&mut self, v: Option<Location>) {
		self.context.origin = v
	}
	pub fn original_origin(&self) -> &Location {
		&self.original_origin
	}
	pub fn set_original_origin(&mut self, v: Location) {
		self.original_origin = v
	}
	pub fn trader(&self) -> &Config::Trader {
		&self.trader
	}
	pub fn set_trader(&mut self, v: Config::Trader) {
		self.trader = v
	}
	pub fn error(&self) -> &Option<(u32, XcmError)> {
		&self.error
	}
	pub fn set_error(&mut self, v: Option<(u32, XcmError)>) {
		self.error = v
	}
	pub fn total_surplus(&self) -> &Weight {
		&self.total_surplus
	}
	pub fn set_total_surplus(&mut self, v: Weight) {
		self.total_surplus = v
	}
	pub fn total_refunded(&self) -> &Weight {
		&self.total_refunded
	}
	pub fn set_total_refunded(&mut self, v: Weight) {
		self.total_refunded = v
	}
	pub fn error_handler(&self) -> &Xcm<Config::RuntimeCall> {
		&self.error_handler
	}
	pub fn set_error_handler(&mut self, v: Xcm<Config::RuntimeCall>) {
		self.error_handler = v
	}
	pub fn error_handler_weight(&self) -> &Weight {
		&self.error_handler_weight
	}
	pub fn set_error_handler_weight(&mut self, v: Weight) {
		self.error_handler_weight = v
	}
	pub fn appendix(&self) -> &Xcm<Config::RuntimeCall> {
		&self.appendix
	}
	pub fn set_appendix(&mut self, v: Xcm<Config::RuntimeCall>) {
		self.appendix = v
	}
	pub fn appendix_weight(&self) -> &Weight {
		&self.appendix_weight
	}
	pub fn set_appendix_weight(&mut self, v: Weight) {
		self.appendix_weight = v
	}
	pub fn transact_status(&self) -> &MaybeErrorCode {
		&self.transact_status
	}
	pub fn set_transact_status(&mut self, v: MaybeErrorCode) {
		self.transact_status = v
	}
	pub fn fees_mode(&self) -> &FeesMode {
		&self.fees_mode
	}
	pub fn set_fees_mode(&mut self, v: FeesMode) {
		self.fees_mode = v
	}
	pub fn fees(&self) -> &AssetsInHolding {
		&self.fees
	}
	pub fn set_fees(&mut self, value: AssetsInHolding) {
		self.fees = value;
	}
	pub fn topic(&self) -> &Option<[u8; 32]> {
		&self.context.topic
	}
	pub fn set_topic(&mut self, v: Option<[u8; 32]>) {
		self.context.topic = v;
	}
	pub fn asset_claimer(&self) -> Option<Location> {
		self.asset_claimer.clone()
	}
	pub fn set_message_weight(&mut self, weight: Weight) {
		self.message_weight = weight;
	}
	pub fn already_paid_fees(&self) -> bool {
		self.already_paid_fees
	}
}

pub struct WeighedMessage<Call>(Weight, Xcm<Call>);
impl<C> PreparedMessage for WeighedMessage<C> {
	fn weight_of(&self) -> Weight {
		self.0
	}
}

#[cfg(any(test, feature = "std"))]
impl<C> WeighedMessage<C> {
	pub fn new(weight: Weight, message: Xcm<C>) -> Self {
		Self(weight, message)
	}
}

impl<Config: config::Config> ExecuteXcm<Config::RuntimeCall> for XcmExecutor<Config> {
	type Prepared = WeighedMessage<Config::RuntimeCall>;
	fn prepare(
		mut message: Xcm<Config::RuntimeCall>,
		weight_limit: Weight,
	) -> Result<Self::Prepared, InstructionError> {
		match Config::Weigher::weight(&mut message, weight_limit) {
			Ok(weight) => Ok(WeighedMessage(weight, message)),
			Err(error) => {
				tracing::debug!(
					target: "xcm::prepare",
					?error,
					?message,
					"Failed to calculate weight for XCM message; execution aborted"
				);
				Err(error)
			},
		}
	}
	fn execute(
		origin: impl Into<Location>,
		WeighedMessage(xcm_weight, mut message): WeighedMessage<Config::RuntimeCall>,
		id: &mut XcmHash,
		weight_credit: Weight,
	) -> Outcome {
		let origin = origin.into();
		tracing::trace!(
			target: "xcm::execute",
			?origin,
			?message,
			?id,
			?weight_credit,
			"Executing message",
		);
		let mut properties = Properties { weight_credit, message_id: None };

		// We only want to record under certain conditions (mainly only during dry-running),
		// so as to not degrade regular performance.
		if Config::XcmRecorder::should_record() {
			Config::XcmRecorder::record(message.clone().into());
		}

		if let Err(e) = Config::Barrier::should_execute(
			&origin,
			message.inner_mut(),
			xcm_weight,
			&mut properties,
		) {
			tracing::trace!(
				target: "xcm::execute",
				?origin,
				?message,
				?properties,
				error = ?e,
				"Barrier blocked execution",
			);

			return Outcome::Incomplete {
				used: xcm_weight, // Weight consumed before the error
				error: InstructionError { index: 0, error: XcmError::Barrier }, // The error that occurred
			};
		}

		*id = properties.message_id.unwrap_or(*id);

		let mut vm = Self::new(origin, *id);
		vm.message_weight = xcm_weight;

		while !message.0.is_empty() {
			let result = vm.process(message);
			tracing::trace!(target: "xcm::execute", ?result, "Message executed");
			message = match result {
				Err(error) => {
					vm.total_surplus.saturating_accrue(error.weight);
					vm.error = Some((error.index, error.xcm_error));
					vm.take_error_handler().or_else(|| vm.take_appendix())
				},
				Ok(()) => {
					vm.drop_error_handler();
					vm.take_appendix()
				},
			}
		}

		vm.post_process(xcm_weight)
	}

	fn charge_fees(origin: impl Into<Location>, fees: Assets) -> XcmResult {
		let origin = origin.into();
		if !Config::FeeManager::is_waived(Some(&origin), FeeReason::ChargeFees) {
			let mut charged = AssetsInHolding::new();
			for asset in fees.inner() {
				let withdrawn = Config::AssetTransactor::withdraw_asset(&asset, &origin, None)?;
				charged.subsume_assets(withdrawn);
			}
			Config::FeeManager::handle_fee(charged, None, FeeReason::ChargeFees);
		}
		Ok(())
	}
}

impl<Config: config::Config> XcmAssetTransfers for XcmExecutor<Config> {
	type IsReserve = Config::IsReserve;
	type IsTeleporter = Config::IsTeleporter;
	type AssetTransactor = Config::AssetTransactor;
}

impl<Config: config::Config> FeeManager for XcmExecutor<Config> {
	fn is_waived(origin: Option<&Location>, r: FeeReason) -> bool {
		Config::FeeManager::is_waived(origin, r)
	}

	fn handle_fee(fee: AssetsInHolding, context: Option<&XcmContext>, r: FeeReason) {
		Config::FeeManager::handle_fee(fee, context, r)
	}
}

#[derive(Debug, PartialEq)]
pub struct ExecutorError {
	pub index: u32,
	pub xcm_error: XcmError,
	pub weight: Weight,
}

#[cfg(feature = "runtime-benchmarks")]
impl From<ExecutorError> for frame_benchmarking::BenchmarkError {
	fn from(error: ExecutorError) -> Self {
		tracing::error!(
			index = ?error.index,
			xcm_error = ?error.xcm_error,
			weight = ?error.weight,
			"XCM ERROR",
		);
		Self::Stop("xcm executor error: see error logs")
	}
}

impl<Config: config::Config> XcmExecutor<Config> {
	pub fn new(origin: impl Into<Location>, message_id: XcmHash) -> Self {
		let origin = origin.into();
		Self {
			holding: AssetsInHolding::new(),
			holding_limit: Config::MaxAssetsIntoHolding::get() as usize,
			context: XcmContext { origin: Some(origin.clone()), message_id, topic: None },
			original_origin: origin,
			trader: Config::Trader::new(),
			error: None,
			total_surplus: Weight::zero(),
			total_refunded: Weight::zero(),
			error_handler: Xcm(vec![]),
			error_handler_weight: Weight::zero(),
			appendix: Xcm(vec![]),
			appendix_weight: Weight::zero(),
			transact_status: Default::default(),
			fees_mode: FeesMode { jit_withdraw: false },
			fees: AssetsInHolding::new(),
			asset_used_in_buy_execution: None,
			message_weight: Weight::zero(),
			asset_claimer: None,
			already_paid_fees: false,
			_config: PhantomData,
		}
	}

	/// Execute any final operations after having executed the XCM message.
	/// This includes refunding surplus weight, trapping extra holding funds, and returning any
	/// errors during execution.
	pub fn post_process(mut self, xcm_weight: Weight) -> Outcome {
		// We silently drop any error from our attempt to refund the surplus as it's a charitable
		// thing so best-effort is all we will do.
		let _ = self.refund_surplus();
		drop(self.trader);

		let mut weight_used = xcm_weight.saturating_sub(self.total_surplus);

		if !self.holding.is_empty() {
			tracing::trace!(
				target: "xcm::post_process",
				holding_register = ?self.holding,
				context = ?self.context,
				original_origin = ?self.original_origin,
				"Trapping assets in holding register",
			);
			let claimer = self
				.asset_claimer
				.as_ref()
				.or(self.context.origin.as_ref())
				.unwrap_or(&self.original_origin);
			let trap_weight = Config::AssetTrap::drop_assets(claimer, self.holding, &self.context);
			weight_used.saturating_accrue(trap_weight);
		};

		match self.error {
			None => Outcome::Complete { used: weight_used },
			// TODO: #2841 #REALWEIGHT We should deduct the cost of any instructions following
			// the error which didn't end up being executed.
			Some((index, error)) => {
				tracing::trace!(
					target: "xcm::post_process",
					instruction = ?index,
					?error,
					original_origin = ?self.original_origin,
					"Execution failed",
				);
				Outcome::Incomplete {
					used: weight_used,
					error: InstructionError { index: index.try_into().unwrap_or(u8::MAX), error },
				}
			},
		}
	}

	fn origin_ref(&self) -> Option<&Location> {
		self.context.origin.as_ref()
	}

	fn cloned_origin(&self) -> Option<Location> {
		self.context.origin.clone()
	}

	/// Send an XCM, charging fees from Holding as needed.
	/// Note: Should be called under a transactional processor to ensure storage noop on failures.
	fn send(
		&mut self,
		dest: Location,
		msg: Xcm<()>,
		reason: FeeReason,
	) -> Result<XcmHash, XcmError> {
		let mut msg = msg;
		// Only the last `SetTopic` instruction is considered relevant. If the message does not end
		// with it, a `topic_or_message_id()` from the context is appended to it. This behaviour is
		// then consistent with `WithUniqueTopic`.
		if !matches!(msg.last(), Some(SetTopic(_))) {
			let topic_id = self.context.topic_or_message_id();
			msg.0.push(SetTopic(topic_id.into()));
		}
		tracing::trace!(
			target: "xcm::send",
			?msg,
			destination = ?dest,
			reason = ?reason,
			"Sending msg",
		);
		let (ticket, fee) = validate_send::<Config::XcmSender>(dest.clone(), msg)?;
		self.take_fee(fee, reason)?;
		match Config::XcmSender::deliver(ticket) {
			Ok(message_id) => {
				Config::XcmEventEmitter::emit_sent_event(
					self.original_origin.clone(),
					dest,
					None, /* Avoid logging the full XCM message to prevent inconsistencies and
					       * reduce storage usage. */
					message_id,
				);
				Ok(message_id)
			},
			Err(error) => {
				tracing::debug!(target: "xcm::send", ?error, "XCM failed to deliver with error");
				Config::XcmEventEmitter::emit_send_failure_event(
					self.original_origin.clone(),
					dest,
					error.clone(),
					self.context.topic_or_message_id(),
				);
				Err(error.into())
			},
		}
	}

	/// Remove the registered error handler and return it. Do not refund its weight.
	fn take_error_handler(&mut self) -> Xcm<Config::RuntimeCall> {
		let mut r = Xcm::<Config::RuntimeCall>(vec![]);
		core::mem::swap(&mut self.error_handler, &mut r);
		self.error_handler_weight = Weight::zero();
		r
	}

	/// Drop the registered error handler and refund its weight.
	fn drop_error_handler(&mut self) {
		self.error_handler = Xcm::<Config::RuntimeCall>(vec![]);
		self.total_surplus.saturating_accrue(self.error_handler_weight);
		self.error_handler_weight = Weight::zero();
	}

	/// Remove the registered appendix and return it.
	fn take_appendix(&mut self) -> Xcm<Config::RuntimeCall> {
		let mut r = Xcm::<Config::RuntimeCall>(vec![]);
		core::mem::swap(&mut self.appendix, &mut r);
		self.appendix_weight = Weight::zero();
		r
	}

	fn ensure_can_subsume_assets(&self, assets_length: usize) -> Result<(), XcmError> {
		// worst-case, holding.len becomes 2 * holding_limit.
		// this guarantees that if holding.len() == holding_limit and you have more than
		// `holding_limit` items (which has a best case outcome of holding.len() == holding_limit),
		// then the operation is guaranteed to succeed.
		let worst_case_holding_len = self.holding.len() + assets_length;
		tracing::trace!(
			target: "xcm::ensure_can_subsume_assets",
			?worst_case_holding_len,
			holding_limit = ?self.holding_limit,
			"Ensuring subsume assets work",
		);
		ensure!(worst_case_holding_len <= self.holding_limit * 2, XcmError::HoldingWouldOverflow);
		Ok(())
	}

	/// Refund any unused weight.
	fn refund_surplus(&mut self) -> Result<(), XcmError> {
		let current_surplus = self.total_surplus.saturating_sub(self.total_refunded);
		tracing::trace!(
			target: "xcm::refund_surplus",
			total_surplus = ?self.total_surplus,
			total_refunded = ?self.total_refunded,
			?current_surplus,
			"Refunding surplus",
		);
		if current_surplus.any_gt(Weight::zero()) {
			if let Some(refund) = self.trader.refund_weight(current_surplus, &self.context) {
				// Check if adding the refund would overflow holding. This can happen if the
				// refund asset is not already in holding and holding is at max capacity.
				if refund
					.fungible
					.first_key_value()
					.map(|(id, _)| {
						!self.holding.fungible.contains_key(id) &&
							self.ensure_can_subsume_assets(1).is_err()
					})
					.unwrap_or(false)
				{
					// Can't add refund to holding - undo by buying back the weight.
					// This returns the refund credit to the trader where it will be
					// handled by OnUnbalanced when the trader is dropped.
					let _ = self
						.trader
						.buy_weight(current_surplus, refund, &self.context)
						.defensive_proof(
							"refund_weight returned an asset capable of buying weight; qed",
						);
					tracing::error!(
						target: "xcm::refund_surplus",
						"error: HoldingWouldOverflow",
					);
					return Err(XcmError::HoldingWouldOverflow);
				}
				self.total_refunded.saturating_accrue(current_surplus);
				self.holding.subsume_assets(refund);
			}
		}
		// If there are any leftover `fees`, merge them with `holding`.
		if !self.fees.is_empty() {
			let leftover_fees = self.fees.saturating_take(Wild(All));
			tracing::trace!(
				target: "xcm::refund_surplus",
				?leftover_fees,
			);
			self.holding.subsume_assets(leftover_fees);
		}
		tracing::trace!(
			target: "xcm::refund_surplus",
			total_refunded = ?self.total_refunded,
		);
		Ok(())
	}

	/// Takes `fees` from holding or fees registers.
	/// Note: Should be called under a transactional processor to ensure storage noop on failures.
	fn take_fee(&mut self, fees: Assets, reason: FeeReason) -> XcmResult {
		if Config::FeeManager::is_waived(self.origin_ref(), reason.clone()) {
			return Ok(());
		}
		tracing::trace!(
			target: "xcm::fees",
			?fees,
			origin_ref = ?self.origin_ref(),
			fees_mode = ?self.fees_mode,
			?reason,
			"Taking fees",
		);
		// We only ever use the first asset from `fees`.
		let Some(asset_needed_for_fees) = fees.get(0) else {
			return Ok(()); // No delivery fees need to be paid.
		};
		// If `BuyExecution` or `PayFees` was called, we use that asset for delivery fees as well.
		let asset_to_pay_for_fees =
			self.calculate_asset_for_delivery_fees(asset_needed_for_fees.clone());
		tracing::trace!(target: "xcm::fees", ?asset_to_pay_for_fees);
		// We withdraw or take from holding the asset the user wants to use for fee payment.
		let withdrawn_fee_asset: AssetsInHolding = if self.fees_mode.jit_withdraw {
			let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
			let credit = Config::AssetTransactor::withdraw_asset(
				&asset_to_pay_for_fees,
				origin,
				Some(&self.context),
			)?;
			tracing::trace!(target: "xcm::fees", ?asset_needed_for_fees);
			credit
		} else {
			// This condition exists to support `BuyExecution` while the ecosystem
			// transitions to `PayFees`.
			let assets_to_pay_delivery_fees: AssetsInHolding = if self.fees.is_empty() {
				// Means `BuyExecution` was used, we'll find the fees in the `holding` register.
				self.holding.try_take(asset_to_pay_for_fees.clone().into()).map_err(|e| {
					tracing::error!(target: "xcm::fees", ?e, ?asset_to_pay_for_fees,
							"Holding doesn't hold enough for fees");
					XcmError::NotHoldingFees
				})?
			} else {
				// Means `PayFees` was used, we'll find the fees in the `fees` register.
				self.fees.try_take(asset_to_pay_for_fees.clone().into()).map_err(|e| {
					tracing::error!(target: "xcm::fees", ?e, ?asset_to_pay_for_fees,
							"Fees register doesn't hold enough for fees");
					XcmError::NotHoldingFees
				})?
			};
			tracing::trace!(target: "xcm::fees", ?assets_to_pay_delivery_fees);
			assets_to_pay_delivery_fees
		};
		// We perform the swap, if needed, to pay fees.
		let paid = if asset_to_pay_for_fees.id != asset_needed_for_fees.id {
			Config::AssetExchanger::exchange_asset(
				self.origin_ref(),
				withdrawn_fee_asset,
				&asset_needed_for_fees.clone().into(),
				false,
			)
			.map_err(|given_assets| {
				tracing::error!(
					target: "xcm::fees",
					?given_assets, ?asset_needed_for_fees, "Swap was deemed necessary but couldn't be done:",
				);
				self.fees.subsume_assets(given_assets);
				XcmError::FeesNotMet
			})?
		} else {
			// If the asset wanted to pay for fees is the one that was needed,
			// we don't need to do any swap.
			// We just use the assets withdrawn or taken from holding.
			withdrawn_fee_asset
		};
		Config::FeeManager::handle_fee(paid, Some(&self.context), reason);
		Ok(())
	}

	/// Calculates the amount of asset used in `PayFees` or `BuyExecution` that would be
	/// charged for swapping to `asset_needed_for_fees`.
	///
	/// The calculation is done by `Config::AssetExchanger`.
	/// If neither `PayFees` or `BuyExecution` were used, or no swap is required,
	/// it will just return `asset_needed_for_fees`.
	fn calculate_asset_for_delivery_fees(&self, asset_needed_for_fees: Asset) -> Asset {
		let Some(asset_wanted_for_fees) =
			// we try to swap first asset in the fees register (should only ever be one),
			self.fees.fungible.first_key_value().map(|(id, _)| id).or_else(|| {
				// or the one used in BuyExecution
				self.asset_used_in_buy_execution.as_ref()
			})
			// if it is different than what we need
			.filter(|&id| asset_needed_for_fees.id.ne(id))
		else {
			// either nothing to swap or we're already holding the right asset
			return asset_needed_for_fees
		};
		Config::AssetExchanger::quote_exchange_price(
			&(asset_wanted_for_fees.clone(), Fungible(0)).into(),
			&asset_needed_for_fees.clone().into(),
			false, // Minimal.
		)
		.and_then(|necessary_assets| {
			// We only use the first asset for fees.
			// If this is not enough to swap for the fee asset then it will error later down
			// the line.
			necessary_assets.into_inner().into_iter().next()
		})
		.unwrap_or_else(|| {
			// If we can't convert, then we return the original asset.
			// It will error later in any case.
			tracing::trace!(
				target: "xcm::calculate_asset_for_delivery_fees",
				?asset_wanted_for_fees, "Could not convert fees",
			);
			asset_needed_for_fees
		})
	}

	/// Calculates what `local_querier` would be from the perspective of `destination`.
	fn to_querier(
		local_querier: Option<Location>,
		destination: &Location,
	) -> Result<Option<Location>, XcmError> {
		Ok(match local_querier {
			None => None,
			Some(q) => Some(
				q.reanchored(&destination, &Config::UniversalLocation::get()).map_err(|e| {
					tracing::error!(target: "xcm::xcm_executor::to_querier", ?e, ?destination, "Failed to re-anchor local_querier");
					XcmError::ReanchorFailed
				})?,
			),
		})
	}

	/// Send a bare `QueryResponse` message containing `response` informed by the given `info`.
	///
	/// The `local_querier` argument is the querier (if any) specified from the *local* perspective.
	fn respond(
		&mut self,
		local_querier: Option<Location>,
		response: Response,
		info: QueryResponseInfo,
		fee_reason: FeeReason,
	) -> Result<XcmHash, XcmError> {
		let querier = Self::to_querier(local_querier, &info.destination)?;
		let QueryResponseInfo { destination, query_id, max_weight } = info;
		let instruction = QueryResponse { query_id, response, max_weight, querier };
		let message = Xcm(vec![instruction]);
		self.send(destination, message, fee_reason)
	}

	fn do_reserve_deposit_assets(
		assets: AssetsInHolding,
		dest: &Location,
		remote_xcm: &mut Vec<Instruction<()>>,
		context: Option<&XcmContext>,
	) -> Result<Assets, XcmError> {
		let reanchored_assets = Self::reanchored_assets(&assets, dest);
		Self::deposit_assets_with_retry(assets, dest, context)?;
		remote_xcm.push(ReserveAssetDeposited(reanchored_assets.clone()));

		Ok(reanchored_assets)
	}

	fn do_reserve_withdraw_assets(
		assets: AssetsInHolding,
		failed_bin: &mut AssetsInHolding,
		reserve: &Location,
		remote_xcm: &mut Vec<Instruction<()>>,
	) -> Result<Assets, XcmError> {
		// Must ensure that we recognise the assets as being managed by the destination.
		#[cfg(not(any(test, feature = "runtime-benchmarks")))]
		for asset in assets.assets_iter() {
			ensure!(
				Config::IsReserve::contains(&asset, &reserve),
				XcmError::UntrustedReserveLocation
			);
		}
		// Note that here we are able to place any assets which could not be
		// reanchored back into Holding (failed_bin).
		let reanchored_assets =
			assets.reanchor_and_burn_local(reserve, &Config::UniversalLocation::get(), failed_bin);
		remote_xcm.push(WithdrawAsset(reanchored_assets.clone()));

		Ok(reanchored_assets)
	}

	fn do_teleport_assets(
		assets: AssetsInHolding,
		dest: &Location,
		remote_xcm: &mut Vec<Instruction<()>>,
		context: &XcmContext,
	) -> Result<Assets, XcmError> {
		let reanchored_assets = Self::reanchored_assets(&assets, dest);
		for asset in assets.assets_iter() {
			// Must ensure that we have teleport trust with destination for these assets.
			#[cfg(not(any(test, feature = "runtime-benchmarks")))]
			ensure!(
				Config::IsTeleporter::contains(&asset, &dest),
				XcmError::UntrustedTeleportLocation
			);
			// We should check that the asset can actually be teleported out (for
			// this to be in error, there would need to be an accounting violation
			// by ourselves, so it's unlikely, but we don't want to allow that kind
			// of bug to leak into a trusted chain.
			Config::AssetTransactor::can_check_out(dest, &asset, context)?;
		}
		for asset in assets.assets_iter() {
			Config::AssetTransactor::check_out(dest, &asset, context);
		}
		remote_xcm.push(ReceiveTeleportedAsset(reanchored_assets.clone()));

		Ok(reanchored_assets)
	}

	fn try_reanchor<T: Reanchorable>(
		reanchorable: T,
		destination: &Location,
	) -> Result<(T, InteriorLocation), XcmError> {
		let reanchor_context = Config::UniversalLocation::get();
		let reanchored =
			reanchorable.reanchored(&destination, &reanchor_context).map_err(|error| {
				tracing::error!(target: "xcm::reanchor", ?error, ?destination, ?reanchor_context, "Failed reanchoring with error.");
				XcmError::ReanchorFailed
			})?;
		Ok((reanchored, reanchor_context))
	}

	/// NOTE: Any assets which were unable to be reanchored are introduced into `failed_bin`.
	fn reanchored_assets(assets: &AssetsInHolding, dest: &Location) -> Assets {
		assets.reanchored_assets(dest, &Config::UniversalLocation::get())
	}

	#[cfg(any(test, feature = "runtime-benchmarks"))]
	pub fn bench_process(&mut self, xcm: Xcm<Config::RuntimeCall>) -> Result<(), ExecutorError> {
		self.process(xcm)
	}

	#[cfg(any(test, feature = "runtime-benchmarks"))]
	pub fn bench_post_process(self, xcm_weight: Weight) -> Outcome {
		self.post_process(xcm_weight)
	}

	fn process(&mut self, xcm: Xcm<Config::RuntimeCall>) -> Result<(), ExecutorError> {
		tracing::trace!(
			target: "xcm::process",
			origin = ?self.origin_ref(),
			total_surplus = ?self.total_surplus,
			total_refunded = ?self.total_refunded,
			error_handler_weight = ?self.error_handler_weight,
		);
		let mut result = Ok(());
		for (i, mut instr) in xcm.0.into_iter().enumerate() {
			match &mut result {
				r @ Ok(()) => {
					// Initialize the recursion count only the first time we hit this code in our
					// potential recursive execution.
					let inst_res = recursion_count::using_once(&mut 1, || {
						recursion_count::with(|count| {
							if *count > RECURSION_LIMIT {
								return None;
							}
							*count = count.saturating_add(1);
							Some(())
						})
						.flatten()
						.ok_or(XcmError::ExceedsStackLimit)?;

						// Ensure that we always decrement the counter whenever we finish processing
						// the instruction.
						defer! {
							recursion_count::with(|count| {
								*count = count.saturating_sub(1);
							});
						}

						self.process_instruction(instr)
					});
					if let Err(error) = inst_res {
						tracing::debug!(
							target: "xcm::process",
							?error, "XCM execution failed at instruction index={i}"
						);
						Config::XcmEventEmitter::emit_process_failure_event(
							self.original_origin.clone(),
							error,
							self.context.topic_or_message_id(),
						);
						*r = Err(ExecutorError {
							index: i as u32,
							xcm_error: error,
							weight: Weight::zero(),
						});
					}
				},
				Err(ref mut error) => {
					if let Ok(x) = Config::Weigher::instr_weight(&mut instr) {
						error.weight.saturating_accrue(x)
					}
				},
			}
		}
		result
	}

	/// Execute `f` inside a transactional context that backs up and restores `holding` and
	/// `fees` on failure.
	fn transactional_process(
		&mut self,
		f: impl FnOnce(&mut Self) -> Result<(), XcmError>,
	) -> Result<(), XcmError> {
		self.transactional_process_with_custom_rollback(f, |_| {})
	}

	/// Like [`Self::transactional_process`], but also calls `on_rollback` when the
	/// transaction is rolled back.
	///
	/// NOTE: holding and fees registers are already automatically rolled back. Custom handler
	/// is for _extra_ rollback logic.
	fn transactional_process_with_custom_rollback(
		&mut self,
		f: impl FnOnce(&mut Self) -> Result<(), XcmError>,
		on_rollback: impl FnOnce(&mut Self),
	) -> Result<(), XcmError> {
		let mut backup_holding = BackupAssetsInHolding::safe_backup(&self.holding);
		let mut backup_fees = BackupAssetsInHolding::safe_backup(&self.fees);
		let result = Config::TransactionalProcessor::process(|| f(self));
		if Config::TransactionalProcessor::IS_TRANSACTIONAL && result.is_err() {
			backup_holding.restore_into(&mut self.holding);
			backup_fees.restore_into(&mut self.fees);
			on_rollback(self);
		}
		result
	}

	/// Process a single XCM instruction, mutating the state of the XCM virtual machine.
	fn process_instruction(
		&mut self,
		instr: Instruction<Config::RuntimeCall>,
	) -> Result<(), XcmError> {
		tracing::trace!(
			target: "xcm::process_instruction",
			instruction = ?instr,
			"Processing instruction",
		);

		match instr {
			WithdrawAsset(assets) => {
				self.ensure_can_subsume_assets(assets.len())?;
				Config::TransactionalProcessor::process(|| {
					let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
					let mut total_surplus = Weight::zero();
					let mut withdrawn = AssetsInHolding::new();
					// Take `assets` from the origin account (on-chain)...
					for asset in assets.inner() {
						let (credit, surplus) = Config::AssetTransactor::withdraw_asset_with_surplus(
							asset,
							origin,
							Some(&self.context),
						)?;
						withdrawn.subsume_assets(credit);
						// If we have some surplus, aggregate it.
						total_surplus.saturating_accrue(surplus);
					}
					// ...and place into holding.
					self.holding.subsume_assets(withdrawn);
					// Credit the total surplus.
					self.total_surplus.saturating_accrue(total_surplus);
					Ok(())
				})
			},
			ReserveAssetDeposited(assets) => {
				self.ensure_can_subsume_assets(assets.len())?;
				Config::TransactionalProcessor::process(|| {
					// Check whether we trust origin to be our reserve location for this asset.
					let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
					// Collect all minted assets first, then add to holding atomically.
					// This ensures partial mints don't pollute holding if a later mint fails. If one of them does fail,
					// TransactionalProcessor makes sure the imbalance changes do not get committed.
					let mut minted_assets = AssetsInHolding::new();
					for asset in assets.inner() {
						// Must ensure that we recognise the asset as being managed by the origin.
						ensure!(
							Config::IsReserve::contains(asset, origin),
							XcmError::UntrustedReserveLocation
						);
						Config::AssetTransactor::mint_asset(asset, &self.context)
							.map(|minted| minted_assets.subsume_assets(minted))?;
					}
					self.holding.subsume_assets(minted_assets);
					Ok(())
				})
			},
			TransferAsset { assets, beneficiary } => {
				Config::TransactionalProcessor::process(|| {
					// Take `assets` from the origin account (on-chain) and place into dest account.
					let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
					let mut total_surplus = Weight::zero();
					for asset in assets.inner() {
						let (_, surplus) = Config::AssetTransactor::transfer_asset_with_surplus(
							&asset,
							origin,
							&beneficiary,
							&self.context,
						)?;
						// If we have some surplus, aggregate it.
						total_surplus.saturating_accrue(surplus);
					}
					// Credit the total surplus.
					self.total_surplus.saturating_accrue(total_surplus);
					Ok(())
				})
			},
			TransferReserveAsset { mut assets, dest, xcm } => {
				self.transactional_process(|self_ref| {
					let origin = self_ref.origin_ref().ok_or(XcmError::BadOrigin)?;
					let mut total_surplus = Weight::zero();
					// Take `assets` from the origin account (on-chain) and place into dest account.
					for asset in assets.inner() {
						let (_, surplus) = Config::AssetTransactor::transfer_asset_with_surplus(
							asset,
							origin,
							&dest,
							&self_ref.context,
						)?;
						// If we have some surplus, aggregate it.
						total_surplus.saturating_accrue(surplus);
					}
					let reanchor_context = Config::UniversalLocation::get();
					assets
						.reanchor(&dest, &reanchor_context)
						.map_err(|()| {
							tracing::debug!(
								target: "xcm::process_instruction::transfer_reserve_asset",
								?assets,
								?dest,
								?reanchor_context,
								"Failed to reanchor assets to destination in context"
							);
							XcmError::LocationFull
						})?;
					let mut message = vec![ReserveAssetDeposited(assets), ClearOrigin];
					message.extend(xcm.0.into_iter());
					self_ref.send(dest, Xcm(message), FeeReason::TransferReserveAsset)?;
					// Credit the total surplus.
					self_ref.total_surplus.saturating_accrue(total_surplus);
					Ok(())
				})
			},
			ReceiveTeleportedAsset(assets) => {
				self.ensure_can_subsume_assets(assets.len())?;
				Config::TransactionalProcessor::process(|| {
					let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
					let mut minted_assets = AssetsInHolding::new();
					// Check whether we trust origin to teleport this asset to us via config trait.
					for asset in assets.inner() {
						// We only trust the origin to send us assets that they identify as their
						// sovereign assets.
						ensure!(
							Config::IsTeleporter::contains(asset, origin),
							XcmError::UntrustedTeleportLocation
						);
						// We should check that the asset can actually be teleported in (for this to
						// be in error, there would need to be an accounting violation by one of the
						// trusted chains, so it's unlikely, but we don't want to punish a possibly
						// innocent chain/user).
						Config::AssetTransactor::can_check_in(origin, asset, &self.context)?;
						Config::AssetTransactor::check_in(origin, asset, &self.context);
						Config::AssetTransactor::mint_asset(asset, &self.context)
							.map(|minted| minted_assets.subsume_assets(minted))?;
					}
					self.holding.subsume_assets(minted_assets);
					Ok(())
				})
			},
			// `fallback_max_weight` is not used in the executor, it's only for conversions.
			Transact { origin_kind, call, .. } => {
				let origin = self.cloned_origin().ok_or_else(|| {
					tracing::trace!(
						target: "xcm::process_instruction::transact",
						"No origin provided",
					);

					XcmError::BadOrigin
				})?;

				let message_call = call.try_into().map_err(|_| {
					tracing::trace!(
						target: "xcm::process_instruction::transact",
						"Failed to decode call",
					);

					XcmError::FailedToDecode
				})?;

				tracing::trace!(
					target: "xcm::process_instruction::transact",
					?message_call,
					"Processing call",
				);

				if !Config::SafeCallFilter::contains(&message_call) {
					tracing::trace!(
						target: "xcm::process_instruction::transact",
						"Call filtered by `SafeCallFilter`",
					);

					return Err(XcmError::NoPermission)
				}

				let dispatch_origin =
					Config::OriginConverter::convert_origin(origin.clone(), origin_kind).map_err(
						|_| {
							tracing::trace!(
								target: "xcm::process_instruction::transact",
								?origin,
								?origin_kind,
								"Failed to convert origin to a local origin."
							);

							XcmError::BadOrigin
						},
					)?;

				tracing::trace!(
					target: "xcm::process_instruction::transact",
					origin = ?dispatch_origin,
					call = ?message_call,
					"Dispatching call with origin",
				);

				let weight = message_call.get_dispatch_info().call_weight;
				let maybe_actual_weight =
					match Config::CallDispatcher::dispatch(message_call, dispatch_origin) {
						Ok(post_info) => {
							tracing::trace!(
								target: "xcm::process_instruction::transact",
								?post_info,
								"Dispatch successful"
							);
							self.transact_status = MaybeErrorCode::Success;
							post_info.actual_weight
						},
						Err(error_and_info) => {
							tracing::trace!(
								target: "xcm::process_instruction::transact",
								?error_and_info,
								"Dispatch failed"
							);

							self.transact_status = error_and_info.error.encode().into();
							error_and_info.post_info.actual_weight
						},
					};
				let actual_weight = maybe_actual_weight.unwrap_or(weight);
				let surplus = weight.saturating_sub(actual_weight);
				// If the actual weight of the call was less than the specified weight, we credit it.
				//
				// We make the adjustment for the total surplus, which is used eventually
				// reported back to the caller and this ensures that they account for the total
				// weight consumed correctly (potentially allowing them to do more operations in a
				// block than they otherwise would).
				self.total_surplus.saturating_accrue(surplus);
				Ok(())
			},
			QueryResponse { query_id, response, max_weight, querier } => {
				let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
				Config::ResponseHandler::on_response(
					origin,
					query_id,
					querier.as_ref(),
					response,
					max_weight,
					&self.context,
				);
				Ok(())
			},
			DescendOrigin(who) => self.do_descend_origin(who),
			ClearOrigin => self.do_clear_origin(),
			ExecuteWithOrigin { .. } => Err(XcmError::Unimplemented),
			ReportError(response_info) => {
				// Report the given result by sending a QueryResponse XCM to a previously given
				// outcome destination if one was registered.
				self.respond(
					self.cloned_origin(),
					Response::ExecutionResult(self.error),
					response_info,
					FeeReason::Report,
				)?;
				Ok(())
			},
			DepositAsset { assets, beneficiary } => {
				self.transactional_process(|self_ref| {
					let deposited = self_ref.holding.saturating_take(assets);
					let surplus = Self::deposit_assets_with_retry(
						deposited,
						&beneficiary,
						Some(&self_ref.context),
					)?;
					self_ref.total_surplus.saturating_accrue(surplus);
					Ok(())
				})
			},
			DepositReserveAsset { assets, dest, xcm } => {
				self.transactional_process(|self_ref| {
					let mut assets = self_ref.holding.saturating_take(assets);
					// When not using `PayFees`, nor `JIT_WITHDRAW`, delivery fees are paid from
					// transferred assets.
					let maybe_delivery_fee_from_assets = if self_ref.fees.is_empty() && !self_ref.fees_mode.jit_withdraw {
						// Deduct and return the part of `assets` that shall be used for delivery fees.
						self_ref.take_delivery_fee_from_assets(&mut assets, &dest, FeeReason::DepositReserveAsset, &xcm)?
					} else {
						None
					};
					let mut message = Vec::with_capacity(xcm.len() + 2);
					tracing::trace!(target: "xcm::DepositReserveAsset", ?assets, "Assets except delivery fee");
					Self::do_reserve_deposit_assets(
						assets,
						&dest,
						&mut message,
						Some(&self_ref.context),
					)?;
					// clear origin for subsequent custom instructions
					message.push(ClearOrigin);
					// append custom instructions
					message.extend(xcm.0.into_iter());
					if let Some(delivery_fee) = maybe_delivery_fee_from_assets {
						// Put back delivery_fee in holding register to be charged by XcmSender.
						self_ref.holding.subsume_assets(delivery_fee);
					}
					self_ref.send(dest, Xcm(message), FeeReason::DepositReserveAsset)?;
					Ok(())
				})
			},
			InitiateReserveWithdraw { assets, reserve, xcm } => {
				self.transactional_process(|self_ref| {
					let mut assets = self_ref.holding.saturating_take(assets);
					// When not using `PayFees`, nor `JIT_WITHDRAW`, delivery fees are paid from
					// transferred assets.
					let maybe_delivery_fee_from_assets = if self_ref.fees.is_empty() && !self_ref.fees_mode.jit_withdraw {
						// Deduct and return the part of `assets` that shall be used for delivery fees.
						self_ref.take_delivery_fee_from_assets(&mut assets, &reserve, FeeReason::InitiateReserveWithdraw, &xcm)?
					} else {
						None
					};
					let mut message = Vec::with_capacity(xcm.len() + 2);
					Self::do_reserve_withdraw_assets(
						assets,
						&mut self_ref.holding,
						&reserve,
						&mut message,
					)?;
					// clear origin for subsequent custom instructions
					message.push(ClearOrigin);
					// append custom instructions
					message.extend(xcm.0.into_iter());
					if let Some(delivery_fee) = maybe_delivery_fee_from_assets {
						// Put back delivery_fee in holding register to be charged by XcmSender.
						self_ref.holding.subsume_assets(delivery_fee);
					}
					self_ref.send(reserve, Xcm(message), FeeReason::InitiateReserveWithdraw)?;
					Ok(())
				})
			},
			InitiateTeleport { assets, dest, xcm } => {
				self.transactional_process(|self_ref| {
					let mut assets = self_ref.holding.saturating_take(assets);
					// When not using `PayFees`, nor `JIT_WITHDRAW`, delivery fees are paid from
					// transferred assets.
					let maybe_delivery_fee_from_assets = if self_ref.fees.is_empty() && !self_ref.fees_mode.jit_withdraw {
						// Deduct and return the part of `assets` that shall be used for delivery fees.
						self_ref.take_delivery_fee_from_assets(&mut assets, &dest, FeeReason::InitiateTeleport, &xcm)?
					} else {
						None
					};
					let mut message = Vec::with_capacity(xcm.len() + 2);
					Self::do_teleport_assets(assets, &dest, &mut message, &self_ref.context)?;
					// clear origin for subsequent custom instructions
					message.push(ClearOrigin);
					// append custom instructions
					message.extend(xcm.0.into_iter());
					if let Some(delivery_fee) = maybe_delivery_fee_from_assets {
						// Put back delivery_fee in holding register to be charged by XcmSender.
						self_ref.holding.subsume_assets(delivery_fee);
					}
					self_ref.send(dest.clone(), Xcm(message), FeeReason::InitiateTeleport)?;
					Ok(())
				})
			},
			InitiateTransfer { destination, remote_fees, preserve_origin, assets, remote_xcm } => {
				self.transactional_process(|self_ref| {
					let mut message = Vec::with_capacity(assets.len() + remote_xcm.len() + 2);

					// We need to transfer the fees and buy execution on remote chain _BEFORE_
					// transferring the other assets. This is required to satisfy the
					// `MAX_ASSETS_FOR_BUY_EXECUTION` limit in the `AllowTopLevelPaidExecutionFrom`
					// barrier.
					let remote_fees_paid = if let Some(remote_fees) = remote_fees {
						let reanchored_fees = match remote_fees {
							AssetTransferFilter::Teleport(fees_filter) => {
								let teleport_fees = self_ref
									.holding
									.try_take(fees_filter)
									.map_err(|error| {
										tracing::debug!(
											target: "xcm::process_instruction::initiate_transfer", ?error,
											"Failed to take specified teleport fees from holding"
										);
										XcmError::NotHoldingFees
									})?;
								Self::do_teleport_assets(
									teleport_fees,
									&destination,
									&mut message,
									&self_ref.context,
								)?
							},
							AssetTransferFilter::ReserveDeposit(fees_filter) => {
								let reserve_deposit_fees = self_ref
									.holding
									.try_take(fees_filter)
									.map_err(|error| {
										tracing::debug!(
											target: "xcm::process_instruction::initiate_transfer", ?error,
											"Failed to take specified reserve deposit fees from holding"
										);
										XcmError::NotHoldingFees
									})?;
								Self::do_reserve_deposit_assets(
									reserve_deposit_fees,
									&destination,
									&mut message,
									Some(&self_ref.context),
								)?
							},
							AssetTransferFilter::ReserveWithdraw(fees_filter) => {
								let reserve_withdraw_fees = self_ref
									.holding
									.try_take(fees_filter)
									.map_err(|error| {
										tracing::debug!(
											target: "xcm::process_instruction::initiate_transfer", ?error,
											"Failed to take specified reserve withdraw fees from holding"
										);
										XcmError::NotHoldingFees
									})?;
								Self::do_reserve_withdraw_assets(
									reserve_withdraw_fees,
									&mut self_ref.holding,
									&destination,
									&mut message,
								)?
							},
						};
						ensure!(reanchored_fees.len() == 1, XcmError::TooManyAssets);
						let fees =
							reanchored_fees.into_inner().pop().ok_or(XcmError::NotHoldingFees)?;
						// move these assets to the fees register for covering execution and paying
						// any subsequent fees
						message.push(PayFees { asset: fees });
						true
					} else {
						false
					};

					// add any extra asset transfers
					for asset_filter in assets {
						match asset_filter {
							AssetTransferFilter::Teleport(assets) => Self::do_teleport_assets(
								self_ref.holding.saturating_take(assets),
								&destination,
								&mut message,
								&self_ref.context,
							)?,
							AssetTransferFilter::ReserveDeposit(assets) =>
								Self::do_reserve_deposit_assets(
									self_ref.holding.saturating_take(assets),
									&destination,
									&mut message,
									Some(&self_ref.context),
								)?,
							AssetTransferFilter::ReserveWithdraw(assets) =>
								Self::do_reserve_withdraw_assets(
									self_ref.holding.saturating_take(assets),
									&mut self_ref.holding,
									&destination,
									&mut message,
								)?,
						};
					}

					match self_ref
						.origin_ref() {
						Some(origin) if preserve_origin => {
							// We alias the origin if it's not a noop (origin != `Here`).
							if *origin != Location::here() {
								// preserve current origin for subsequent user-controlled instructions on
								// remote chain
								let reanchored_origin = Self::try_reanchor(origin.clone(), &destination)?.0;
								message.push(AliasOrigin(reanchored_origin));
							}
							// If origin is Location::here() and we want to preserve it, we don't alter.
						}
						_ => {
							// clear origin for subsequent user-controlled instructions on remote chain
							message.push(ClearOrigin);
						}
					}

					// If not intending to pay for fees then we append the `UnpaidExecution`
					// _AFTER_ origin altering instructions.
					// When origin is not preserved, it's probably going to fail on the receiver.
					if !remote_fees_paid {
						// We push the UnpaidExecution instruction to notify we do not intend to pay
						// for fees.
						// The receiving chain must decide based on the origin of the message if they
						// accept this.
						message
							.push(UnpaidExecution { weight_limit: Unlimited, check_origin: None });
					}

					// append custom instructions
					message.extend(remote_xcm.0.into_iter());
					// send the onward XCM
					self_ref.send(destination, Xcm(message), FeeReason::InitiateTransfer)?;
					Ok(())
				})
			},
			ReportHolding { response_info, assets } => {
				let context = Config::UniversalLocation::get();
				let assets = self.holding.min(&assets)
					.into_inner()
					.into_iter()
					.filter_map(|a| a.reanchored(&response_info.destination, &context).ok())
					.collect::<Vec<Asset>>()
					.into();
				self.respond(
					self.cloned_origin(),
					Response::Assets(assets),
					response_info,
					FeeReason::Report,
				)?;
				Ok(())
			},
			BuyExecution { fees, weight_limit } => {
				// There is no need to buy any weight if `weight_limit` is `Unlimited` since it
				// would indicate that `AllowTopLevelPaidExecutionFrom` was unused for execution
				// and thus there is some other reason why it has been determined that this XCM
				// should be executed.
				let Some(weight) = Option::<Weight>::from(weight_limit) else { return Ok(()) };
				// Save the asset being used for execution fees, so we later know what should be
				// used for delivery fees.
				self.asset_used_in_buy_execution = Some(fees.id.clone());
				tracing::trace!(
					target: "xcm::executor::BuyExecution",
					asset_used_in_buy_execution = ?self.asset_used_in_buy_execution
				);
				self.transactional_process(|self_ref| {
					// pay for `weight` using up to `fees` of the holding register.
					let max_fee =
						self_ref.holding.try_take(fees.clone().into()).map_err(|e| {
							tracing::error!(target: "xcm::process_instruction::buy_execution", ?e, ?fees,
							"Failed to take fees from holding");
							XcmError::NotHoldingFees
						})?;
					let unspent = self_ref.trader.buy_weight(weight, max_fee, &self_ref.context).map_err(|(unspent, e)| {
						self_ref.holding.subsume_assets(unspent);
						e
					})?;
					self_ref.holding.subsume_assets(unspent);
					Ok(())
				})
			},
			PayFees { asset } => {
				// If we've already paid for fees, do nothing.
				if self.already_paid_fees {
					return Ok(());
				}
				// Make sure `PayFees` won't be processed again.
				self.already_paid_fees = true;
				// The max we're willing to pay for fees is decided by the `asset` operand.
				tracing::trace!(
					target: "xcm::executor::PayFees",
					asset_for_fees = ?asset,
					message_weight = ?self.message_weight,
				);
				// Pay for execution fees.
				self.transactional_process_with_custom_rollback(
					|self_ref| {
						let max_fee =
							self_ref.holding.try_take(asset.into()).map_err(|error| {
								tracing::debug!(
									target: "xcm::process_instruction::pay_fees", ?error,
									"Failed to take fees from holding"
								);
								XcmError::NotHoldingFees
							})?;
						let unspent =
							self_ref.trader.buy_weight(self_ref.message_weight, max_fee.into(), &self_ref.context).map_err(|(unspent, e)| {
								self_ref.fees.subsume_assets(unspent);
								e
							})?;
						// Move unspent to the `fees` register, it can later be moved to holding by calling `RefundSurplus`.
						self_ref.fees.subsume_assets(unspent);
						Ok(())
					},
					|self_ref| {
						self_ref.already_paid_fees = false;
					},
				)
			},
			RefundSurplus => self.refund_surplus(),
			SetErrorHandler(mut handler) => {
				let handler_weight = Config::Weigher::weight(&mut handler, Weight::MAX)
					.map_err(|error| {
						tracing::debug!(
							target: "xcm::executor::SetErrorHandler",
							?error,
							?handler,
							"Failed to calculate weight"
						);
						XcmError::WeightNotComputable
					})?;
				self.total_surplus.saturating_accrue(self.error_handler_weight);
				self.error_handler = handler;
				self.error_handler_weight = handler_weight;
				Ok(())
			},
			SetAppendix(mut appendix) => {
				let appendix_weight = Config::Weigher::weight(&mut appendix, Weight::MAX)
					.map_err(|error| {
						tracing::debug!(
							target: "xcm::executor::SetErrorHandler",
							?error,
							?appendix,
							"Failed to calculate weight"
						);
						XcmError::WeightNotComputable
					})?;
				self.total_surplus.saturating_accrue(self.appendix_weight);
				self.appendix = appendix;
				self.appendix_weight = appendix_weight;
				Ok(())
			},
			ClearError => {
				self.error = None;
				Ok(())
			},
			SetHints { hints } => {
				for hint in hints.into_iter() {
					match hint {
						AssetClaimer { location } => {
							self.asset_claimer = Some(location)
						},
					}
				}
				Ok(())
			},
			ClaimAsset { assets, ticket } => {
				let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
				self.ensure_can_subsume_assets(assets.len())?;
				let claimed = Config::AssetTrap::claim_assets(origin, &ticket, &assets, &self.context);
				self.holding.subsume_assets(claimed.ok_or(XcmError::UnknownClaim)?);
				Ok(())
			},
			Trap(code) => Err(XcmError::Trap(code)),
			SubscribeVersion { query_id, max_response_weight } => {
				let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
				// We don't allow derivative origins to subscribe since it would otherwise pose a
				// DoS risk.
				ensure!(&self.original_origin == origin, XcmError::BadOrigin);
				Config::SubscriptionService::start(
					origin,
					query_id,
					max_response_weight,
					&self.context,
				)
			},
			UnsubscribeVersion => {
				let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
				ensure!(&self.original_origin == origin, XcmError::BadOrigin);
				Config::SubscriptionService::stop(origin, &self.context)
			},
			BurnAsset(assets) => {
				self.holding.saturating_take(assets.into());
				Ok(())
			},
			ExpectAsset(assets) =>
				self.holding.ensure_contains(&assets).map_err(|e| {
					tracing::error!(target: "xcm::process_instruction::expect_asset", ?e, ?assets, "assets not contained in holding");
					XcmError::ExpectationFalse
				}),
			ExpectOrigin(origin) => {
				ensure!(self.context.origin == origin, XcmError::ExpectationFalse);
				Ok(())
			},
			ExpectError(error) => {
				ensure!(self.error == error, XcmError::ExpectationFalse);
				Ok(())
			},
			ExpectTransactStatus(transact_status) => {
				ensure!(self.transact_status == transact_status, XcmError::ExpectationFalse);
				Ok(())
			},
			QueryPallet { module_name, response_info } => {
				let pallets = Config::PalletInstancesInfo::infos()
					.into_iter()
					.filter(|x| x.module_name.as_bytes() == &module_name[..])
					.map(|x| {
						PalletInfo::new(
							x.index as u32,
							x.name.as_bytes().into(),
							x.module_name.as_bytes().into(),
							x.crate_version.major as u32,
							x.crate_version.minor as u32,
							x.crate_version.patch as u32,
						)
					})
					.collect::<Result<Vec<_>, XcmError>>()?;
				let QueryResponseInfo { destination, query_id, max_weight } = response_info;
				let response =
					Response::PalletsInfo(pallets.try_into().map_err(|error| {
						tracing::debug!(
							target: "xcm::process_instruction::query_pallet", ?error,
							"Failed to convert pallets to response info"
						);
						XcmError::Overflow
					})?);
				let querier = Self::to_querier(self.cloned_origin(), &destination)?;
				let instruction = QueryResponse { query_id, response, max_weight, querier };
				let message = Xcm(vec![instruction]);
				self.send(destination, message, FeeReason::QueryPallet)?;
				Ok(())
			},
			ExpectPallet { index, name, module_name, crate_major, min_crate_minor } => {
				let pallet = Config::PalletInstancesInfo::infos()
					.into_iter()
					.find(|x| x.index == index as usize)
					.ok_or(XcmError::PalletNotFound)?;
				ensure!(pallet.name.as_bytes() == &name[..], XcmError::NameMismatch);
				ensure!(pallet.module_name.as_bytes() == &module_name[..], XcmError::NameMismatch);
				let major = pallet.crate_version.major as u32;
				ensure!(major == crate_major, XcmError::VersionIncompatible);
				let minor = pallet.crate_version.minor as u32;
				ensure!(minor >= min_crate_minor, XcmError::VersionIncompatible);
				Ok(())
			},
			ReportTransactStatus(response_info) => {
				self.respond(
					self.cloned_origin(),
					Response::DispatchResult(self.transact_status.clone()),
					response_info,
					FeeReason::Report,
				)?;
				Ok(())
			},
			ClearTransactStatus => {
				self.transact_status = Default::default();
				Ok(())
			},
			UniversalOrigin(new_global) => {
				let universal_location = Config::UniversalLocation::get();
				ensure!(universal_location.first() != Some(&new_global), XcmError::InvalidLocation);
				let origin = self.cloned_origin().ok_or(XcmError::BadOrigin)?;
				let origin_xform = (origin, new_global);
				let ok = Config::UniversalAliases::contains(&origin_xform);
				ensure!(ok, XcmError::InvalidLocation);
				let (_, new_global) = origin_xform;
				let new_origin = Junctions::from([new_global]).relative_to(&universal_location);
				self.context.origin = Some(new_origin);
				Ok(())
			},
			ExportMessage { network, destination, xcm } => {
				// The actual message sent to the bridge for forwarding is prepended with
				// `UniversalOrigin` and `DescendOrigin` in order to ensure that the message is
				// executed with this Origin.
				//
				// Prepend the desired message with instructions which effectively rewrite the
				// origin.
				//
				// This only works because the remote chain empowers the bridge
				// to speak for the local network.
				let origin = self.context.origin.as_ref().ok_or(XcmError::BadOrigin)?.clone();
				let universal_source = Config::UniversalLocation::get()
					.within_global(origin)
					.map_err(|()| {
						tracing::debug!(
							target: "xcm::process_instruction::export_message",
							"Failed to reanchor origin to universal location",
						);
						XcmError::Unanchored
					})?;
				let hash = (self.origin_ref(), &destination).using_encoded(blake2_128);
				let channel = u32::decode(&mut hash.as_ref()).unwrap_or(0);
				// Hash identifies the lane on the exporter which we use. We use the pairwise
				// combination of the origin and destination to ensure origin/destination pairs
				// will generally have their own lanes.
				let (ticket, fee) = validate_export::<Config::MessageExporter>(
					network,
					channel,
					universal_source,
					destination.clone(),
					xcm,
				)?;
				self.transactional_process(|self_ref| {
					self_ref.take_fee(fee, FeeReason::Export { network, destination })?;
					let _ = Config::MessageExporter::deliver(ticket).defensive_proof(
						"`deliver` called immediately after `validate_export`; \
						`take_fee` does not affect the validity of the ticket; qed",
					);
					Ok(())
				})
			},
			LockAsset { asset, unlocker } => {
				self.transactional_process(|self_ref| {
					let origin = self_ref.cloned_origin().ok_or(XcmError::BadOrigin)?;
					let (remote_asset, context) = Self::try_reanchor(asset.clone(), &unlocker)?;
					let lock_ticket =
						Config::AssetLocker::prepare_lock(unlocker.clone(), asset, origin.clone())?;
					let owner = origin.reanchored(&unlocker, &context).map_err(|e| {
						tracing::error!(target: "xcm::xcm_executor::process_instruction", ?e, ?unlocker, ?context, "Failed to re-anchor origin");
						XcmError::ReanchorFailed
					})?;
					let msg = Xcm::<()>(vec![NoteUnlockable { asset: remote_asset, owner }]);
					let (ticket, price) = validate_send::<Config::XcmSender>(unlocker, msg)?;
					self_ref.take_fee(price, FeeReason::LockAsset)?;
					lock_ticket.enact()?;
					Config::XcmSender::deliver(ticket)?;
					Ok(())
				})
			},
			UnlockAsset { asset, target } => {
				let origin = self.cloned_origin().ok_or(XcmError::BadOrigin)?;
				Config::AssetLocker::prepare_unlock(origin, asset, target)?.enact()?;
				Ok(())
			},
			NoteUnlockable { asset, owner } => {
				let origin = self.cloned_origin().ok_or(XcmError::BadOrigin)?;
				Config::AssetLocker::note_unlockable(origin, asset, owner)?;
				Ok(())
			},
			RequestUnlock { asset, locker } => {
				let origin = self.cloned_origin().ok_or(XcmError::BadOrigin)?;
				let remote_asset = Self::try_reanchor(asset.clone(), &locker)?.0;
				let remote_target = Self::try_reanchor(origin.clone(), &locker)?.0;
				let reduce_ticket = Config::AssetLocker::prepare_reduce_unlockable(
					locker.clone(),
					asset,
					origin.clone(),
				)?;
				let msg =
					Xcm::<()>(vec![UnlockAsset { asset: remote_asset, target: remote_target }]);
				let (ticket, price) = validate_send::<Config::XcmSender>(locker, msg)?;
				self.transactional_process(|self_ref| {
					self_ref.take_fee(price, FeeReason::RequestUnlock)?;
					reduce_ticket.enact()?;
					Config::XcmSender::deliver(ticket)?;
					Ok(())
				})
			},
			ExchangeAsset { give, want, maximal } => {
				self.transactional_process(|self_ref| {
					let give = self_ref.holding.saturating_take(give);
					self_ref.ensure_can_subsume_assets(want.len())?;
					let received = Config::AssetExchanger::exchange_asset(
						self_ref.origin_ref(),
						give,
						&want,
						maximal,
					).map_err(|unspent| {
						self_ref.holding.subsume_assets(unspent);
						XcmError::NoDeal
					})?;
					self_ref.holding.subsume_assets(received);
					Ok(())
				})
			},
			SetFeesMode { jit_withdraw } => {
				self.fees_mode = FeesMode { jit_withdraw };
				Ok(())
			},
			SetTopic(topic) => {
				self.context.topic = Some(topic);
				Ok(())
			},
			ClearTopic => {
				self.context.topic = None;
				Ok(())
			},
			AliasOrigin(target) => {
				let origin = self.origin_ref().ok_or(XcmError::BadOrigin)?;
				ensure!(Config::Aliasers::contains(origin, &target), XcmError::NoPermission);
				self.context.origin = Some(target);
				Ok(())
			},
			UnpaidExecution { check_origin, .. } => {
				ensure!(
					check_origin.is_none() || self.context.origin == check_origin,
					XcmError::BadOrigin
				);
				Ok(())
			},
			HrmpNewChannelOpenRequest { sender, max_message_size, max_capacity } =>
				Config::TransactionalProcessor::process(|| {
					Config::HrmpNewChannelOpenRequestHandler::handle(
						sender,
						max_message_size,
						max_capacity,
					)
				}),
			HrmpChannelAccepted { recipient } => Config::TransactionalProcessor::process(|| {
				Config::HrmpChannelAcceptedHandler::handle(recipient)
			}),
			HrmpChannelClosing { initiator, sender, recipient } =>
				Config::TransactionalProcessor::process(|| {
					Config::HrmpChannelClosingHandler::handle(initiator, sender, recipient)
				}),
		}
	}

	fn do_descend_origin(&mut self, who: InteriorLocation) -> XcmResult {
		self.context
			.origin
			.as_mut()
			.ok_or(XcmError::BadOrigin)?
			.append_with(who)
			.map_err(|e| {
				tracing::error!(target: "xcm::do_descend_origin", ?e, "Failed to append junctions");
				XcmError::LocationFull
			})
	}

	fn do_clear_origin(&mut self) -> XcmResult {
		self.context.origin = None;
		Ok(())
	}

	/// Deposit `to_deposit` assets to `beneficiary`, without giving up on the first (transient)
	/// error, and retrying once just in case one of the subsequently deposited assets satisfy some
	/// requirement.
	///
	/// Most common transient error is: `beneficiary` account does not yet exist and the first
	/// asset(s) in the (sorted) list does not satisfy ED, but a subsequent one in the list does.
	///
	/// Any per-asset failure on the retry pass propagates as `Err`, and the surrounding
	/// `transactional_process` rolls back the whole instruction (storage changes are reverted by
	/// `Config::TransactionalProcessor`, and `self.holding` is restored from its
	/// pre-instruction backup). Anything left in `self.holding` after the program finishes is
	/// then trapped by `post_process` via `Config::AssetTrap::drop_assets`, so funds are never
	/// silently lost.
	///
	/// This function can write into storage and also return an error at the same time, it should
	/// always be called within a transactional context.
	fn deposit_assets_with_retry(
		to_deposit: AssetsInHolding,
		beneficiary: &Location,
		context: Option<&XcmContext>,
	) -> Result<Weight, XcmError> {
		let mut total_surplus = Weight::zero();
		let mut failed_deposits = AssetsInHolding::new();

		// First pass: try to deposit each asset; failures go to retry.
		for single in to_deposit.into_per_asset_holdings() {
			match Config::AssetTransactor::deposit_asset_with_surplus(single, beneficiary, context)
			{
				Ok(surplus) => total_surplus.saturating_accrue(surplus),
				Err((unspent, _)) => {
					// First-pass failure: keep for retry. A subsequent deposit in the same
					// pass may create the destination account (by satisfying ED), allowing
					// the retry pass to succeed for assets that fall here.
					failed_deposits.subsume_assets(unspent);
				},
			}
		}

		// Retry previously failed deposits, this time short-circuiting on any error.
		for single in failed_deposits.into_per_asset_holdings() {
			let surplus =
				Config::AssetTransactor::deposit_asset_with_surplus(single, beneficiary, context)
					.map_err(|(unspent, error)| {
					tracing::debug!(
						target: "xcm::deposit_assets_with_retry",
						?error,
						?unspent,
						"Retry-pass deposit failed"
					);
					error
				})?;
			total_surplus.saturating_accrue(surplus);
		}

		Ok(total_surplus)
	}

	/// Take from transferred `assets` the delivery fee required to send an onward transfer message
	/// to `destination`.
	///
	/// Will be removed once the transition from `BuyExecution` to `PayFees` is complete.
	fn take_delivery_fee_from_assets(
		&self,
		assets: &mut AssetsInHolding,
		destination: &Location,
		reason: FeeReason,
		xcm: &Xcm<()>,
	) -> Result<Option<AssetsInHolding>, XcmError> {
		let to_weigh_reanchored = Self::reanchored_assets(&assets, destination);
		let remote_instruction = match reason {
			FeeReason::DepositReserveAsset => ReserveAssetDeposited(to_weigh_reanchored),
			FeeReason::InitiateReserveWithdraw => WithdrawAsset(to_weigh_reanchored),
			FeeReason::InitiateTeleport => ReceiveTeleportedAsset(to_weigh_reanchored),
			_ => {
				tracing::debug!(
					target: "xcm::take_delivery_fee_from_assets",
					"Unexpected delivery fee reason",
				);
				return Err(XcmError::NotHoldingFees);
			},
		};
		let mut message_to_weigh = Vec::with_capacity(xcm.len() + 2);
		message_to_weigh.push(remote_instruction);
		message_to_weigh.push(ClearOrigin);
		message_to_weigh.extend(xcm.0.clone().into_iter());
		let (_, fee) =
			validate_send::<Config::XcmSender>(destination.clone(), Xcm(message_to_weigh))?;
		let maybe_delivery_fee = fee.get(0).map(|asset_needed_for_fees| {
			tracing::trace!(
				target: "xcm::fees::take_delivery_fee_from_assets",
				"Asset provided to pay for fees {:?}, asset required for delivery fees: {:?}",
				self.asset_used_in_buy_execution, asset_needed_for_fees,
			);
			let asset_to_pay_for_fees =
				self.calculate_asset_for_delivery_fees(asset_needed_for_fees.clone());
			// set aside fee to be charged by XcmSender
			let delivery_fee = assets.saturating_take(asset_to_pay_for_fees.into());
			tracing::trace!(target: "xcm::fees::take_delivery_fee_from_assets", ?delivery_fee);
			delivery_fee
		});
		Ok(maybe_delivery_fee)
	}
}
