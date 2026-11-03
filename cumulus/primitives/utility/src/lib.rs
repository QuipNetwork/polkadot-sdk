// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

//! Helper datatypes for cumulus. This includes the [`ParentAsUmp`] routing type which will route
//! messages into an [`UpwardMessageSender`] if the destination is `Parent`.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{boxed::Box, vec, vec::Vec};
use codec::Encode;
use core::marker::PhantomData;
use cumulus_primitives_core::{MessageSendError, UpwardMessageSender};
use frame_support::{
	defensive,
	traits::{
		tokens::{fungibles, imbalance::UnsafeManualAccounting},
		Get, OnUnbalanced as OnUnbalancedT,
	},
	weights::{Weight, WeightToFee as WeightToFeeT},
};
use pallet_asset_conversion::{QuotePrice, SwapCredit as SwapCreditT};
use polkadot_runtime_common::xcm_sender::PriceForMessageDelivery;
use sp_runtime::traits::Zero;
use xcm::{latest::prelude::*, VersionedLocation, VersionedXcm, WrapVersion};
use xcm_builder::InspectMessageQueues;
use xcm_executor::{
	traits::{MatchesFungibles, WeightTrader},
	AssetsInHolding,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod test_helpers {
	pub use xcm_executor::test_helpers::mock_asset_to_holding as asset_to_holding;
}

/// Xcm router which recognises the `Parent` destination and handles it by sending the message into
/// the given UMP `UpwardMessageSender` implementation. Thus this essentially adapts an
/// `UpwardMessageSender` trait impl into a `SendXcm` trait impl.
///
/// NOTE: This is a pretty dumb "just send it" router; we will probably want to introduce queuing
/// to UMP eventually and when we do, the pallet which implements the queuing will be responsible
/// for the `SendXcm` implementation.
pub struct ParentAsUmp<T, W, P>(PhantomData<(T, W, P)>);

impl<T, W, P> SendXcm for ParentAsUmp<T, W, P>
where
	T: UpwardMessageSender,
	W: WrapVersion,
	P: PriceForMessageDelivery<Id = ()>,
{
	type Ticket = Vec<u8>;

	fn validate(dest: &mut Option<Location>, msg: &mut Option<Xcm<()>>) -> SendResult<Vec<u8>> {
		let d = dest.take().ok_or(SendError::MissingArgument)?;

		if d.contains_parents_only(1) {
			// An upward message for the relay chain.
			let xcm = msg.take().ok_or(SendError::MissingArgument)?;
			let price = P::price_for_delivery((), &xcm);
			let versioned_xcm =
				W::wrap_version(&d, xcm).map_err(|()| SendError::DestinationUnsupported)?;
			versioned_xcm
				.check_is_decodable()
				.map_err(|()| SendError::ExceedsMaxMessageSize)?;
			let data = versioned_xcm.encode();

			// Pre-check with our message sender if everything else is okay.
			T::can_send_upward_message(&data).map_err(Self::map_upward_sender_err)?;

			Ok((data, price))
		} else {
			// Anything else is unhandled. This includes a message that is not meant for us.
			// We need to make sure that dest/msg is not consumed here.
			*dest = Some(d);
			Err(SendError::NotApplicable)
		}
	}

	fn deliver(data: Vec<u8>) -> Result<XcmHash, SendError> {
		let (_, hash) = T::send_upward_message(data).map_err(Self::map_upward_sender_err)?;
		Ok(hash)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful_delivery(location: Option<Location>) {
		if location.as_ref().map_or(false, |l| l.contains_parents_only(1)) {
			T::ensure_successful_delivery();
		}
	}
}

impl<T, W, P> ParentAsUmp<T, W, P> {
	fn map_upward_sender_err(message_send_error: MessageSendError) -> SendError {
		match message_send_error {
			MessageSendError::TooBig => SendError::ExceedsMaxMessageSize,
			e => SendError::Transport(e.into()),
		}
	}
}

impl<T: UpwardMessageSender + InspectMessageQueues, W, P> InspectMessageQueues
	for ParentAsUmp<T, W, P>
{
	fn clear_messages() {
		T::clear_messages();
	}

	fn get_messages() -> Vec<(VersionedLocation, Vec<VersionedXcm<()>>)> {
		T::get_messages()
	}
}

/// Charges for execution in the first asset of those selected for fee payment
/// Only succeeds for Concrete Fungible Assets
/// First tries to convert the this Asset into a local assetId
/// Then charges for this assetId as described by FeeCharger
/// Weight, paid balance, local asset Id and the location is stored for
/// later refund purposes
/// Important: Errors if the Trader is being called twice by 2 BuyExecution instructions
/// Alternatively we could just return payment in the aforementioned case
pub struct TakeFirstAssetTrader<
	AccountId: Eq,
	FeeCharger: ChargeWeightInFungibles<AccountId, Fungibles>,
	Matcher: MatchesFungibles<Fungibles::AssetId, Fungibles::Balance>,
	Fungibles: fungibles::Balanced<AccountId>,
	OnUnbalanced: OnUnbalancedT<fungibles::Credit<AccountId, Fungibles>>,
> {
	/// Accumulated fee paid for XCM execution.
	outstanding_credit: Option<fungibles::Credit<AccountId, Fungibles>>,
	/// The amount of weight bought minus the weight already refunded
	weight_outstanding: Weight,
	_phantom_data: PhantomData<(AccountId, FeeCharger, Matcher, Fungibles, OnUnbalanced)>,
}

impl<
		AccountId: Eq,
		FeeCharger: ChargeWeightInFungibles<AccountId, Fungibles>,
		Matcher: MatchesFungibles<Fungibles::AssetId, Fungibles::Balance>,
		Fungibles: fungibles::Inspect<AccountId, Balance = u128, AssetId: Into<Location> + 'static>
			+ fungibles::Balanced<AccountId, OnDropCredit: 'static, OnDropDebt: 'static>,
		OnUnbalanced: OnUnbalancedT<fungibles::Credit<AccountId, Fungibles>>,
	> WeightTrader for TakeFirstAssetTrader<AccountId, FeeCharger, Matcher, Fungibles, OnUnbalanced>
{
	fn new() -> Self {
		Self {
			outstanding_credit: None,
			weight_outstanding: Weight::zero(),
			_phantom_data: PhantomData,
		}
	}
	// We take first asset
	// Check whether we can convert fee to asset_fee (is_sufficient, min_deposit)
	// If everything goes well, we charge.
	fn buy_weight(
		&mut self,
		weight: Weight,
		mut payment: AssetsInHolding,
		context: &XcmContext,
	) -> Result<AssetsInHolding, (AssetsInHolding, XcmError)> {
		log::trace!(target: "xcm::weight", "TakeFirstAssetTrader::buy_weight weight: {:?}, payment: {:?}, context: {:?}", weight, payment, context);

		// Make sure we don't enter twice
		if self.outstanding_credit.is_some() {
			return Err((payment, XcmError::NotWithdrawable));
		}

		// We take the very first asset from payment
		let Some(used) = payment.fungible_assets_iter().next() else {
			return Err((payment, XcmError::AssetNotFound));
		};

		// Get the local asset id in which we can pay for fees
		let Ok((fungibles_asset_id, _)) = Matcher::matches_fungibles(&used) else {
			return Err((payment, XcmError::AssetNotFound));
		};

		// Calculate how much we should charge in the asset_id for such amount of weight
		// Require at least a payment of minimum_balance
		// Necessary for fully collateral-backed assets
		let required_amount: u128 =
			match FeeCharger::charge_weight_in_fungibles(fungibles_asset_id.clone(), weight)
				.map(|amount| amount.max(Fungibles::minimum_balance(fungibles_asset_id.clone())))
			{
				Ok(a) => a,
				Err(_) => return Err((payment, XcmError::Overflow)),
			};

		// Convert to the same kind of asset, with the required fungible balance
		let required = used.id.into_asset(required_amount.into());

		// Subtract required from payment.
		// Note: `payment` may contain multiple assets, but we only take from the first fungible
		// asset that was matched above. Any remaining assets stay in `payment` and are returned.
		let Some(imbalance) = payment
			.try_take(required.into())
			.ok()
			.and_then(|taken| taken.fungible.into_iter().next().map(|(_, v)| v))
		else {
			return Err((payment, XcmError::TooExpensive));
		};
		// "manually" build the concrete credit and move the imbalance there.
		let mut credit = fungibles::Credit::<AccountId, Fungibles>::zero(fungibles_asset_id);
		credit.saturating_subsume(imbalance);

		// Record weight and credit.
		self.outstanding_credit = Some(credit);
		self.weight_outstanding = weight;

		// return the unused payment
		Ok(payment)
	}

	/// Refunds unused weight back to holding.
	///
	/// Note: This is a best-effort refund. The actual refunded amount may differ from the
	/// weight-equivalent amount due to existential deposit (ED) constraints. Specifically:
	/// - If refunding the full amount would leave less than ED in outstanding credit, we only
	///   refund enough to keep ED for the drop handler.
	/// - This ensures collateral-backed assets always have sufficient balance for proper cleanup.
	fn refund_weight(&mut self, weight: Weight, context: &XcmContext) -> Option<AssetsInHolding> {
		log::trace!(target: "xcm::weight", "TakeFirstAssetTrader::refund_weight weight: {:?}, context: {:?}", weight, context);
		if weight.is_zero() {
			return None;
		}
		let outstanding_credit = self.outstanding_credit.as_mut()?;
		let id = outstanding_credit.asset();
		let fun = Fungible(outstanding_credit.peek());
		let asset = (id.clone(), fun).into();

		// Get the local asset id in which we can refund fees.
		let (fungibles_asset_id, _) = Matcher::matches_fungibles(&asset).ok()?;
		let minimum_balance = Fungibles::minimum_balance(fungibles_asset_id.clone());

		// Calculate how much to refund based on unused weight.
		// This read should have already been cached in buy_weight.
		let refund_credit = FeeCharger::charge_weight_in_fungibles(fungibles_asset_id, weight)
			.ok()
			.map(|refund_balance| {
				// Ensure at least minimum_balance remains for the drop handler.
				// This is necessary for fully collateral-backed assets.
				if outstanding_credit.peek().saturating_sub(refund_balance) >= minimum_balance {
					outstanding_credit.extract(refund_balance)
				} else {
					// Keep at least ED in outstanding credit for the OnUnbalanced drop
					// handler. Refund only the surplus above ED (zero if outstanding < ED).
					let keep = minimum_balance.min(outstanding_credit.peek());
					let refund_amount = outstanding_credit.peek().saturating_sub(keep);
					outstanding_credit.extract(refund_amount)
				}
			})?;
		// Subtract the refunded weight from existing weight.
		self.weight_outstanding = self.weight_outstanding.saturating_sub(weight);

		// Only return refund if non-zero.
		if refund_credit.peek() != Zero::zero() {
			Some(AssetsInHolding::new_from_fungible_credit(asset.id, Box::new(refund_credit)))
		} else {
			None
		}
	}

	fn quote_weight(
		&mut self,
		weight: Weight,
		given_id: AssetId,
		context: &XcmContext,
	) -> Result<Asset, XcmError> {
		log::trace!(
			target: "xcm::weight",
			"TakeFirstAssetTrader::quote_weight weight: {:?}, given_id: {:?}, context: {:?}",
			weight, given_id, context
		);
		if weight.is_zero() {
			return Err(XcmError::NoDeal);
		}

		let give_matcher: Asset = (given_id.clone(), 1).into();
		// Get the local asset id in which we can pay for fees
		let (give_fungibles_id, _) =
			Matcher::matches_fungibles(&give_matcher).map_err(|_| XcmError::AssetNotFound)?;

		// Calculate how much we should charge in the asset_id for such amount of weight
		// Require at least a payment of minimum_balance
		// Necessary for fully collateral-backed assets
		let required_amount: u128 =
			FeeCharger::charge_weight_in_fungibles(give_fungibles_id.clone(), weight)
				.map(|amount| amount.max(Fungibles::minimum_balance(give_fungibles_id.clone())))
				.map_err(|_| XcmError::Overflow)?;

		// Convert to the same kind of asset, with the required fungible balance
		let required = given_id.into_asset(required_amount.into());
		Ok(required)
	}
}

impl<
		AccountId: Eq,
		FeeCharger: ChargeWeightInFungibles<AccountId, Fungibles>,
		Matcher: MatchesFungibles<Fungibles::AssetId, Fungibles::Balance>,
		Fungibles: fungibles::Balanced<AccountId>,
		OnUnbalanced: OnUnbalancedT<fungibles::Credit<AccountId, Fungibles>>,
	> Drop for TakeFirstAssetTrader<AccountId, FeeCharger, Matcher, Fungibles, OnUnbalanced>
{
	fn drop(&mut self) {
		self.outstanding_credit
			.take()
			.filter(|credit| !credit.peek().is_zero())
			.map(OnUnbalanced::on_unbalanced);
	}
}

/// ChargeWeightInFungibles trait, which converts a given amount of weight
/// and an assetId, and it returns the balance amount that should be charged
/// in such assetId for that amount of weight
pub trait ChargeWeightInFungibles<AccountId, Assets: fungibles::Inspect<AccountId>> {
	fn charge_weight_in_fungibles(
		asset_id: <Assets as fungibles::Inspect<AccountId>>::AssetId,
		weight: Weight,
	) -> Result<<Assets as fungibles::Inspect<AccountId>>::Balance, XcmError>;
}

/// Provides an implementation of [`WeightTrader`] to charge for weight using the first asset
/// specified in the `payment` argument.
///
/// The asset used to pay for the weight must differ from the `Target` asset and be exchangeable for
/// the same `Target` asset through `SwapCredit`.
///
/// ### Parameters:
/// - `Target`: the asset into which the user's payment will be exchanged using `SwapCredit`.
/// - `SwapCredit`: mechanism used for the exchange of the user's payment asset into the `Target`.
/// - `WeightToFee`: weight to the `Target` asset fee calculator.
/// - `Fungibles`: registry of fungible assets.
/// - `FungiblesAssetMatcher`: utility for mapping [`Asset`] to `Fungibles::AssetId` and
///   `Fungibles::Balance`.
/// - `OnUnbalanced`: handler for the fee payment.
/// - `AccountId`: the account identifier type.
pub struct SwapFirstAssetTrader<
	Target: Get<Fungibles::AssetId>,
	SwapCredit: SwapCreditT<
			AccountId,
			Balance = Fungibles::Balance,
			AssetKind = Fungibles::AssetId,
			Credit = fungibles::Credit<AccountId, Fungibles>,
		> + QuotePrice,
	WeightToFee: WeightToFeeT<Balance = Fungibles::Balance>,
	Fungibles: fungibles::Balanced<AccountId>,
	FungiblesAssetMatcher: MatchesFungibles<Fungibles::AssetId, Fungibles::Balance>,
	OnUnbalanced: OnUnbalancedT<fungibles::Credit<AccountId, Fungibles>>,
	AccountId,
> where
	Fungibles::Balance: From<u128> + Into<u128>,
{
	/// Accumulated fee paid for XCM execution.
	total_fee: fungibles::Credit<AccountId, Fungibles>,
	/// Last asset utilized by a client to settle a fee.
	last_fee_asset: Option<AssetId>,
	_phantom_data: PhantomData<(
		Target,
		SwapCredit,
		WeightToFee,
		Fungibles,
		FungiblesAssetMatcher,
		OnUnbalanced,
		AccountId,
	)>,
}

impl<
		Target: Get<Fungibles::AssetId>,
		SwapCredit: SwapCreditT<
				AccountId,
				Balance = Fungibles::Balance,
				AssetKind = Fungibles::AssetId,
				Credit = fungibles::Credit<AccountId, Fungibles>,
			> + QuotePrice<AssetKind = Fungibles::AssetId, Balance = Fungibles::Balance>,
		WeightToFee: WeightToFeeT<Balance = Fungibles::Balance>,
		Fungibles: fungibles::Balanced<
			AccountId,
			AssetId: 'static,
			OnDropCredit: 'static,
			OnDropDebt: 'static,
		>,
		FungiblesAssetMatcher: MatchesFungibles<Fungibles::AssetId, Fungibles::Balance>,
		OnUnbalanced: OnUnbalancedT<fungibles::Credit<AccountId, Fungibles>>,
		AccountId,
	> WeightTrader
	for SwapFirstAssetTrader<
		Target,
		SwapCredit,
		WeightToFee,
		Fungibles,
		FungiblesAssetMatcher,
		OnUnbalanced,
		AccountId,
	>
where
	Fungibles::Balance: From<u128> + Into<u128>,
{
	fn new() -> Self {
		Self {
			total_fee: fungibles::Credit::<AccountId, Fungibles>::zero(Target::get()),
			last_fee_asset: None,
			_phantom_data: PhantomData,
		}
	}

	fn buy_weight(
		&mut self,
		weight: Weight,
		mut payment: AssetsInHolding,
		_context: &XcmContext,
	) -> Result<AssetsInHolding, (AssetsInHolding, XcmError)> {
		log::trace!(
			target: "xcm::weight",
			"SwapFirstAssetTrader::buy_weight weight: {:?}, payment: {:?}",
			weight,
			payment,
		);
		let Some((id, given_credit)) = payment.fungible.first_key_value() else {
			return Err((payment, XcmError::AssetNotFound));
		};
		let id = id.clone();
		let given_credit_amount = given_credit.amount();
		let first_asset: Asset = (id.clone(), given_credit_amount).into();
		let Ok((fungibles_id, _)) = FungiblesAssetMatcher::matches_fungibles(&first_asset) else {
			log::trace!(
				target: "xcm::weight",
				"SwapFirstAssetTrader::buy_weight asset {:?} didn't match",
				first_asset,
			);
			return Err((payment, XcmError::AssetNotFound));
		};

		let swap_asset = fungibles_id.clone().into();
		if Target::get().eq(&swap_asset) {
			log::trace!(
				target: "xcm::weight",
				"SwapFirstAssetTrader::buy_weight Asset was same as Target, swap not needed.",
			);
			// current trader is not applicable.
			return Err((payment, XcmError::FeesNotMet));
		}
		// Subtract required from payment
		let Some(imbalance) = payment.fungible.remove(&first_asset.id) else {
			return Err((payment, XcmError::TooExpensive));
		};
		// "manually" build the concrete credit and move the imbalance there.
		let mut credit_in = fungibles::Credit::<AccountId, Fungibles>::zero(fungibles_id);
		credit_in.saturating_subsume(imbalance);

		let fee = WeightToFee::weight_to_fee(&weight);
		// swap the user's asset for the `Target` asset.
		let (credit_out, credit_change) = match SwapCredit::swap_tokens_for_exact_tokens(
			vec![swap_asset, Target::get()],
			credit_in,
			fee,
		) {
			Ok(a) => a,
			Err((credit_in, error)) => {
				log::trace!(
					target: "xcm::weight",
					"SwapFirstAssetTrader::buy_weight swap couldn't be done. Error was: {:?}",
					error,
				);
				// put back the taken credit
				let taken =
					AssetsInHolding::new_from_fungible_credit(id.clone(), Box::new(credit_in));
				payment.subsume_assets(taken);
				return Err((payment, XcmError::FeesNotMet));
			},
		};

		match self.total_fee.subsume(credit_out) {
			Err(credit_out) => {
				// error may occur if `total_fee.asset` differs from `credit_out.asset`, which does
				// not apply in this context.
				defensive!(
					"`total_fee.asset` must be equal to `credit_out.asset`",
					(self.total_fee.asset(), credit_out.asset())
				);
				return Err((payment, XcmError::FeesNotMet));
			},
			_ => (),
		};
		self.last_fee_asset = Some(id.clone());

		if credit_change.peek() != Zero::zero() {
			let unspent = AssetsInHolding::new_from_fungible_credit(id, Box::new(credit_change));
			payment.subsume_assets(unspent);
		}
		Ok(payment)
	}

	fn refund_weight(&mut self, weight: Weight, _context: &XcmContext) -> Option<AssetsInHolding> {
		log::trace!(
			target: "xcm::weight",
			"SwapFirstAssetTrader::refund_weight weight: {:?}, self.total_fee: {:?}",
			weight,
			self.total_fee,
		);
		if weight.is_zero() || self.total_fee.peek().is_zero() {
			// noting to refund.
			return None;
		}
		let refund_asset = if let Some(asset) = &self.last_fee_asset {
			// create an initial zero refund in the asset used in the last `buy_weight`.
			(asset.clone(), Fungible(0)).into()
		} else {
			return None;
		};
		let refund_amount = WeightToFee::weight_to_fee(&weight);
		if refund_amount >= self.total_fee.peek() {
			// not enough was paid to refund the `weight`.
			return None;
		}

		let refund_swap_asset = FungiblesAssetMatcher::matches_fungibles(&refund_asset)
			.map(|(a, _)| a.into())
			.ok()?;

		let refund = self.total_fee.extract(refund_amount);
		let refund = match SwapCredit::swap_exact_tokens_for_tokens(
			vec![Target::get(), refund_swap_asset],
			refund,
			None,
		) {
			Ok(refund_in_target) => refund_in_target,
			Err((refund, _)) => {
				// return an attempted refund back to the `total_fee`.
				let _ = self.total_fee.subsume(refund).map_err(|refund| {
					// error may occur if `total_fee.asset` differs from `refund.asset`, which does
					// not apply in this context.
					defensive!(
						"`total_fee.asset` must be equal to `refund.asset`",
						(self.total_fee.asset(), refund.asset())
					);
				});
				return None;
			},
		};

		let refund = AssetsInHolding::new_from_fungible_credit(refund_asset.id, Box::new(refund));
		Some(refund)
	}

	fn quote_weight(
		&mut self,
		weight: Weight,
		given_id: AssetId,
		_context: &XcmContext,
	) -> Result<Asset, XcmError> {
		log::trace!(
			target: "xcm::weight",
			"SwapFirstAssetTrader::quote_weight weight: {:?}, given_id: {:?}",
			weight,
			given_id,
		);
		if weight.is_zero() {
			return Err(XcmError::NoDeal);
		}

		let give_matcher: Asset = (given_id.clone(), 1).into();
		let (give_fungibles_id, _) = FungiblesAssetMatcher::matches_fungibles(&give_matcher)
			.map_err(|_| XcmError::AssetNotFound)?;
		let want_fungibles_id = Target::get();
		if give_fungibles_id.eq(&want_fungibles_id.clone().into()) {
			return Err(XcmError::FeesNotMet);
		}

		let want_amount = WeightToFee::weight_to_fee(&weight);
		// The `give` amount required to obtain `want`.
		let necessary_give: u128 = <SwapCredit as QuotePrice>::quote_price_tokens_for_exact_tokens(
			give_fungibles_id,
			want_fungibles_id,
			want_amount,
			true, // Include fee.
		)
		.filter(|amount| *amount > 0u128.into())
		.ok_or(XcmError::FeesNotMet)?
		.into();
		Ok((given_id, necessary_give).into())
	}
}

impl<
		Target: Get<Fungibles::AssetId>,
		SwapCredit: SwapCreditT<
				AccountId,
				Balance = Fungibles::Balance,
				AssetKind = Fungibles::AssetId,
				Credit = fungibles::Credit<AccountId, Fungibles>,
			> + QuotePrice,
		WeightToFee: WeightToFeeT<Balance = Fungibles::Balance>,
		Fungibles: fungibles::Balanced<AccountId>,
		FungiblesAssetMatcher: MatchesFungibles<Fungibles::AssetId, Fungibles::Balance>,
		OnUnbalanced: OnUnbalancedT<fungibles::Credit<AccountId, Fungibles>>,
		AccountId,
	> Drop
	for SwapFirstAssetTrader<
		Target,
		SwapCredit,
		WeightToFee,
		Fungibles,
		FungiblesAssetMatcher,
		OnUnbalanced,
		AccountId,
	>
where
	Fungibles::Balance: From<u128> + Into<u128>,
{
	fn drop(&mut self) {
		if self.total_fee.peek().is_zero() {
			return;
		}
		let total_fee = self.total_fee.extract(self.total_fee.peek());
		OnUnbalanced::on_unbalanced(total_fee);
	}
}

#[cfg(test)]
mod test_xcm_router {
	use super::*;
	use cumulus_primitives_core::UpwardMessage;
	use frame_support::assert_ok;
	use xcm::MAX_XCM_DECODE_DEPTH;

	/// Validates [`validate`] for required Some(destination) and Some(message)
	struct OkFixedXcmHashWithAssertingRequiredInputsSender;

	impl OkFixedXcmHashWithAssertingRequiredInputsSender {
		const FIXED_XCM_HASH: [u8; 32] = [9; 32];

		fn fixed_delivery_asset() -> Assets {
			Assets::new()
		}

		fn expected_delivery_result() -> Result<(XcmHash, Assets), SendError> {
			Ok((Self::FIXED_XCM_HASH, Self::fixed_delivery_asset()))
		}
	}

	impl SendXcm for OkFixedXcmHashWithAssertingRequiredInputsSender {
		type Ticket = ();

		fn validate(
			destination: &mut Option<Location>,
			message: &mut Option<Xcm<()>>,
		) -> SendResult<Self::Ticket> {
			assert!(destination.is_some());
			assert!(message.is_some());
			Ok(((), OkFixedXcmHashWithAssertingRequiredInputsSender::fixed_delivery_asset()))
		}

		fn deliver(_: Self::Ticket) -> Result<XcmHash, SendError> {
			Ok(Self::FIXED_XCM_HASH)
		}
	}

	/// Impl [`UpwardMessageSender`] that return `Ok` for `can_send_upward_message`.
	struct CanSendUpwardMessageSender;

	impl UpwardMessageSender for CanSendUpwardMessageSender {
		fn send_upward_message(_: UpwardMessage) -> Result<(u32, XcmHash), MessageSendError> {
			Err(MessageSendError::Other)
		}

		fn can_send_upward_message(_: &UpwardMessage) -> Result<(), MessageSendError> {
			Ok(())
		}
	}

	#[test]
	fn parent_as_ump_does_not_consume_dest_or_msg_on_not_applicable() {
		// dummy message
		let message = Xcm(vec![Trap(5)]);

		// ParentAsUmp - check dest is really not applicable
		let dest = (Parent, Parent, Parent);
		let mut dest_wrapper = Some(dest.into());
		let mut msg_wrapper = Some(message.clone());
		assert_eq!(
			Err(SendError::NotApplicable),
			<ParentAsUmp<(), (), ()> as SendXcm>::validate(&mut dest_wrapper, &mut msg_wrapper)
		);

		// check wrapper were not consumed
		assert_eq!(Some(dest.into()), dest_wrapper.take());
		assert_eq!(Some(message.clone()), msg_wrapper.take());

		// another try with router chain with asserting sender
		assert_eq!(
			OkFixedXcmHashWithAssertingRequiredInputsSender::expected_delivery_result(),
			send_xcm::<(ParentAsUmp<(), (), ()>, OkFixedXcmHashWithAssertingRequiredInputsSender)>(
				dest.into(),
				message,
			)
		);
	}

	#[test]
	fn parent_as_ump_consumes_dest_and_msg_on_ok_validate() {
		// dummy message
		let message = Xcm(vec![Trap(5)]);

		// ParentAsUmp - check dest/msg is valid
		let dest = (Parent, Here);
		let mut dest_wrapper = Some(dest.clone().into());
		let mut msg_wrapper = Some(message.clone());
		assert!(<ParentAsUmp<CanSendUpwardMessageSender, (), ()> as SendXcm>::validate(
			&mut dest_wrapper,
			&mut msg_wrapper,
		)
		.is_ok());

		// check wrapper were consumed
		assert_eq!(None, dest_wrapper.take());
		assert_eq!(None, msg_wrapper.take());

		// another try with router chain with asserting sender
		assert_eq!(
			Err(SendError::Transport("Other")),
			send_xcm::<(
				ParentAsUmp<CanSendUpwardMessageSender, (), ()>,
				OkFixedXcmHashWithAssertingRequiredInputsSender
			)>(dest.into(), message)
		);
	}

	#[test]
	fn parent_as_ump_validate_nested_xcm_works() {
		let dest = Parent;

		type Router = ParentAsUmp<CanSendUpwardMessageSender, (), ()>;

		// Message that is not too deeply nested:
		let mut good = Xcm(vec![ClearOrigin]);
		for _ in 0..MAX_XCM_DECODE_DEPTH - 1 {
			good = Xcm(vec![SetAppendix(good)]);
		}

		// Check that the good message is validated:
		assert_ok!(<Router as SendXcm>::validate(&mut Some(dest.into()), &mut Some(good.clone())));

		// Nesting the message one more time should reject it:
		let bad = Xcm(vec![SetAppendix(good)]);
		assert_eq!(
			Err(SendError::ExceedsMaxMessageSize),
			<Router as SendXcm>::validate(&mut Some(dest.into()), &mut Some(bad))
		);
	}
}

#[cfg(test)]
mod test_trader {
	use super::{test_helpers::asset_to_holding, *};
	use frame_support::{
		assert_ok,
		traits::tokens::{
			DepositConsequence, Fortitude, Preservation, Provenance, WithdrawConsequence,
		},
	};
	use sp_runtime::DispatchError;
	use xcm_builder::TakeRevenue;
	use xcm_executor::traits::Error;

	#[test]
	fn take_first_asset_trader_buy_weight_called_twice_throws_error() {
		const AMOUNT: u128 = 100;

		// prepare prerequisites to instantiate `TakeFirstAssetTrader`
		type TestAccountId = u32;
		type TestAssetId = Location; // Use Location directly as AssetId
		type TestBalance = u128;

		struct TestAssets;
		impl MatchesFungibles<TestAssetId, TestBalance> for TestAssets {
			fn matches_fungibles(a: &Asset) -> Result<(TestAssetId, TestBalance), Error> {
				match a {
					Asset { fun: Fungible(amount), id: AssetId(_id) } => {
						Ok((Location::new(0, [GeneralIndex(1)]), *amount))
					},
					_ => Err(Error::AssetNotHandled),
				}
			}
		}
		impl fungibles::Inspect<TestAccountId> for TestAssets {
			type AssetId = TestAssetId;
			type Balance = TestBalance;

			fn total_issuance(_: Self::AssetId) -> Self::Balance {
				0
			}

			fn minimum_balance(_: Self::AssetId) -> Self::Balance {
				0
			}

			fn balance(_: Self::AssetId, _: &TestAccountId) -> Self::Balance {
				0
			}

			fn total_balance(_: Self::AssetId, _: &TestAccountId) -> Self::Balance {
				0
			}

			fn reducible_balance(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Preservation,
				_: Fortitude,
			) -> Self::Balance {
				0
			}

			fn can_deposit(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
				_: Provenance,
			) -> DepositConsequence {
				DepositConsequence::Success
			}

			fn can_withdraw(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
			) -> WithdrawConsequence<Self::Balance> {
				WithdrawConsequence::Success
			}

			fn asset_exists(_: Self::AssetId) -> bool {
				true
			}
		}
		impl fungibles::Mutate<TestAccountId> for TestAssets {}
		impl fungibles::Balanced<TestAccountId> for TestAssets {
			type OnDropCredit = fungibles::DecreaseIssuance<TestAccountId, Self>;
			type OnDropDebt = fungibles::IncreaseIssuance<TestAccountId, Self>;
		}
		impl fungibles::Unbalanced<TestAccountId> for TestAssets {
			fn handle_dust(_: fungibles::Dust<TestAccountId, Self>) {}
			fn write_balance(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
			) -> Result<Option<Self::Balance>, DispatchError> {
				Ok(None)
			}

			fn set_total_issuance(_: Self::AssetId, _: Self::Balance) {}
		}

		struct FeeChargerAssetsHandleRefund;
		impl ChargeWeightInFungibles<TestAccountId, TestAssets> for FeeChargerAssetsHandleRefund {
			fn charge_weight_in_fungibles(
				_: <TestAssets as fungibles::Inspect<TestAccountId>>::AssetId,
				_: Weight,
			) -> Result<<TestAssets as fungibles::Inspect<TestAccountId>>::Balance, XcmError> {
				Ok(AMOUNT)
			}
		}
		impl TakeRevenue for FeeChargerAssetsHandleRefund {
			fn take_revenue(_: AssetsInHolding) {}
		}

		// Implement OnUnbalanced for the test
		struct HandleFees;
		impl OnUnbalancedT<fungibles::Credit<TestAccountId, TestAssets>> for HandleFees {
			fn on_unbalanced(_: fungibles::Credit<TestAccountId, TestAssets>) {
				// Just drop it for tests
			}
		}

		// create new instance
		type Trader = TakeFirstAssetTrader<
			TestAccountId,
			FeeChargerAssetsHandleRefund,
			TestAssets,
			TestAssets,
			HandleFees,
		>;
		let mut trader = <Trader as WeightTrader>::new();
		let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

		// prepare test data
		let asset: Asset = (Here, AMOUNT).into();
		let payment1 = asset_to_holding(asset.clone());
		let payment2 = asset_to_holding(asset);
		let weight_to_buy = Weight::from_parts(1_000, 1_000);

		// lets do first call (success)
		assert_ok!(trader.buy_weight(weight_to_buy, payment1, &ctx));

		// lets do second call (error)
		let (_, error) = trader.buy_weight(weight_to_buy, payment2, &ctx).unwrap_err();
		assert_eq!(error, XcmError::NotWithdrawable);
	}

	#[test]
	fn take_first_asset_trader_returns_unused_amount() {
		// Regression test for fix: buy_weight should only take the required amount,
		// not the entire balance from payment
		const REQUIRED_AMOUNT: u128 = 100;
		const TOTAL_AMOUNT: u128 = 500; // More than required

		// prepare prerequisites to instantiate `TakeFirstAssetTrader`
		type TestAccountId = u32;
		type TestAssetId = Location;
		type TestBalance = u128;

		struct TestAssets;
		impl MatchesFungibles<TestAssetId, TestBalance> for TestAssets {
			fn matches_fungibles(a: &Asset) -> Result<(TestAssetId, TestBalance), Error> {
				match a {
					Asset { fun: Fungible(amount), id: AssetId(_id) } => {
						Ok((Location::new(0, [GeneralIndex(1)]), *amount))
					},
					_ => Err(Error::AssetNotHandled),
				}
			}
		}
		impl fungibles::Inspect<TestAccountId> for TestAssets {
			type AssetId = TestAssetId;
			type Balance = TestBalance;

			fn total_issuance(_: Self::AssetId) -> Self::Balance {
				0
			}

			fn minimum_balance(_: Self::AssetId) -> Self::Balance {
				0
			}

			fn balance(_: Self::AssetId, _: &TestAccountId) -> Self::Balance {
				0
			}

			fn total_balance(_: Self::AssetId, _: &TestAccountId) -> Self::Balance {
				0
			}

			fn reducible_balance(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Preservation,
				_: Fortitude,
			) -> Self::Balance {
				0
			}

			fn can_deposit(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
				_: Provenance,
			) -> DepositConsequence {
				DepositConsequence::Success
			}

			fn can_withdraw(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
			) -> WithdrawConsequence<Self::Balance> {
				WithdrawConsequence::Success
			}

			fn asset_exists(_: Self::AssetId) -> bool {
				true
			}
		}
		impl fungibles::Mutate<TestAccountId> for TestAssets {}
		impl fungibles::Balanced<TestAccountId> for TestAssets {
			type OnDropCredit = fungibles::DecreaseIssuance<TestAccountId, Self>;
			type OnDropDebt = fungibles::IncreaseIssuance<TestAccountId, Self>;
		}
		impl fungibles::Unbalanced<TestAccountId> for TestAssets {
			fn handle_dust(_: fungibles::Dust<TestAccountId, Self>) {}
			fn write_balance(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
			) -> Result<Option<Self::Balance>, DispatchError> {
				Ok(None)
			}

			fn set_total_issuance(_: Self::AssetId, _: Self::Balance) {}
		}

		struct FeeChargerAssetsHandleRefund;
		impl ChargeWeightInFungibles<TestAccountId, TestAssets> for FeeChargerAssetsHandleRefund {
			fn charge_weight_in_fungibles(
				_: <TestAssets as fungibles::Inspect<TestAccountId>>::AssetId,
				_: Weight,
			) -> Result<<TestAssets as fungibles::Inspect<TestAccountId>>::Balance, XcmError> {
				Ok(REQUIRED_AMOUNT)
			}
		}
		impl TakeRevenue for FeeChargerAssetsHandleRefund {
			fn take_revenue(_: AssetsInHolding) {}
		}

		struct HandleFees;
		impl OnUnbalancedT<fungibles::Credit<TestAccountId, TestAssets>> for HandleFees {
			fn on_unbalanced(_: fungibles::Credit<TestAccountId, TestAssets>) {}
		}

		// create new instance
		type Trader = TakeFirstAssetTrader<
			TestAccountId,
			FeeChargerAssetsHandleRefund,
			TestAssets,
			TestAssets,
			HandleFees,
		>;
		let mut trader = <Trader as WeightTrader>::new();
		let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

		// prepare test data - payment with MORE than required
		let asset: Asset = (Here, TOTAL_AMOUNT).into();
		let payment = asset_to_holding(asset.clone());
		let weight_to_buy = Weight::from_parts(1_000, 1_000);

		// call buy_weight - should succeed and return the excess
		let result = trader.buy_weight(weight_to_buy, payment, &ctx);
		assert_ok!(&result);

		let unused_payment = result.unwrap();

		// verify that the unused payment contains the excess amount
		let expected_excess = TOTAL_AMOUNT - REQUIRED_AMOUNT;
		let unused_assets: Vec<Asset> = unused_payment.fungible_assets_iter().collect();

		// should have exactly one asset remaining
		assert_eq!(unused_assets.len(), 1);

		// verify it's the correct amount (excess)
		match &unused_assets[0] {
			Asset { fun: Fungible(amount), .. } => {
				assert_eq!(*amount, expected_excess, "Expected excess amount to be returned");
			},
			_ => panic!("Expected fungible asset"),
		}
	}

	#[test]
	fn take_first_asset_trader_refund_keeps_ed_for_drop_handler() {
		// Regression test: when refunding would leave less than ED in outstanding
		// credit, `refund_weight` must refund `outstanding - ED` and keep ED for the
		// `OnUnbalanced` drop handler.
		use core::cell::Cell;

		const ED: u128 = 10;
		const BUY_FEE: u128 = 15;
		const REFUND_FEE: u128 = 10;

		type TestAccountId = u32;
		type TestAssetId = Location;
		type TestBalance = u128;

		struct TestAssets;
		impl MatchesFungibles<TestAssetId, TestBalance> for TestAssets {
			fn matches_fungibles(a: &Asset) -> Result<(TestAssetId, TestBalance), Error> {
				match a {
					Asset { fun: Fungible(amount), id: AssetId(_) } => {
						Ok((Location::new(0, [GeneralIndex(1)]), *amount))
					},
					_ => Err(Error::AssetNotHandled),
				}
			}
		}
		impl fungibles::Inspect<TestAccountId> for TestAssets {
			type AssetId = TestAssetId;
			type Balance = TestBalance;
			fn total_issuance(_: Self::AssetId) -> Self::Balance {
				0
			}
			fn minimum_balance(_: Self::AssetId) -> Self::Balance {
				ED
			}
			fn balance(_: Self::AssetId, _: &TestAccountId) -> Self::Balance {
				0
			}
			fn total_balance(_: Self::AssetId, _: &TestAccountId) -> Self::Balance {
				0
			}
			fn reducible_balance(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Preservation,
				_: Fortitude,
			) -> Self::Balance {
				0
			}
			fn can_deposit(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
				_: Provenance,
			) -> DepositConsequence {
				DepositConsequence::Success
			}
			fn can_withdraw(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
			) -> WithdrawConsequence<Self::Balance> {
				WithdrawConsequence::Success
			}
			fn asset_exists(_: Self::AssetId) -> bool {
				true
			}
		}
		impl fungibles::Mutate<TestAccountId> for TestAssets {}
		impl fungibles::Balanced<TestAccountId> for TestAssets {
			type OnDropCredit = fungibles::DecreaseIssuance<TestAccountId, Self>;
			type OnDropDebt = fungibles::IncreaseIssuance<TestAccountId, Self>;
		}
		impl fungibles::Unbalanced<TestAccountId> for TestAssets {
			fn handle_dust(_: fungibles::Dust<TestAccountId, Self>) {}
			fn write_balance(
				_: Self::AssetId,
				_: &TestAccountId,
				_: Self::Balance,
			) -> Result<Option<Self::Balance>, DispatchError> {
				Ok(None)
			}
			fn set_total_issuance(_: Self::AssetId, _: Self::Balance) {}
		}

		// Charge BUY_FEE on the first call (during `buy_weight`), REFUND_FEE on the
		// next (during `refund_weight`).
		thread_local! {
			static CALLS: Cell<u32> = const { Cell::new(0) };
			static HANDLER_RECEIVED: Cell<u128> = const { Cell::new(0) };
		}
		struct FeeCharger;
		impl ChargeWeightInFungibles<TestAccountId, TestAssets> for FeeCharger {
			fn charge_weight_in_fungibles(
				_: <TestAssets as fungibles::Inspect<TestAccountId>>::AssetId,
				_: Weight,
			) -> Result<<TestAssets as fungibles::Inspect<TestAccountId>>::Balance, XcmError> {
				let n = CALLS.with(|c| {
					let v = c.get();
					c.set(v + 1);
					v
				});
				Ok(if n == 0 { BUY_FEE } else { REFUND_FEE })
			}
		}

		struct HandleFees;
		impl OnUnbalancedT<fungibles::Credit<TestAccountId, TestAssets>> for HandleFees {
			fn on_unbalanced(credit: fungibles::Credit<TestAccountId, TestAssets>) {
				HANDLER_RECEIVED.with(|h| h.set(credit.peek()));
			}
		}

		type Trader =
			TakeFirstAssetTrader<TestAccountId, FeeCharger, TestAssets, TestAssets, HandleFees>;
		let mut trader = <Trader as WeightTrader>::new();
		let ctx = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

		// `buy_weight` populates outstanding_credit with BUY_FEE = 15.
		let payment = asset_to_holding((Here, BUY_FEE).into());
		assert_ok!(trader.buy_weight(Weight::from_parts(1_000, 1_000), payment, &ctx));

		// `refund_weight` would compute refund=10, but outstanding(15) - 10 = 5 < ED(10),
		// so it must refund only `outstanding - ED` = 5 and keep ED for the handler.
		let refund = trader
			.refund_weight(Weight::from_parts(500, 500), &ctx)
			.expect("non-zero refund");
		let refund_assets: Vec<Asset> = refund.fungible_assets_iter().collect();
		assert_eq!(refund_assets.len(), 1);
		match &refund_assets[0] {
			Asset { fun: Fungible(amount), .. } => {
				assert_eq!(*amount, BUY_FEE - ED, "refund must be `outstanding - ED`, not ED")
			},
			_ => panic!("expected fungible refund"),
		}

		// Drop the trader; the handler must receive at least ED.
		drop(trader);
		HANDLER_RECEIVED.with(|h| {
			assert_eq!(h.get(), ED, "OnUnbalanced drop handler must receive at least ED")
		});
	}
}

/// Implementation of `xcm_builder::EnsureDelivery` which helps to ensure delivery to the
/// parent relay chain. Deposits existential deposit for origin (if needed).
/// Deposits estimated fee to the origin account (if needed).
/// Allows triggering of additional logic for a specific `ParaId` (e.g. to open an HRMP channel) if
/// needed.
#[cfg(feature = "runtime-benchmarks")]
pub struct ToParentDeliveryHelper<XcmConfig, ExistentialDeposit, PriceForDelivery>(
	core::marker::PhantomData<(XcmConfig, ExistentialDeposit, PriceForDelivery)>,
);

#[cfg(feature = "runtime-benchmarks")]
impl<
		XcmConfig: xcm_executor::Config,
		ExistentialDeposit: Get<Option<Asset>>,
		PriceForDelivery: PriceForMessageDelivery<Id = ()>,
	> xcm_builder::EnsureDelivery
	for ToParentDeliveryHelper<XcmConfig, ExistentialDeposit, PriceForDelivery>
{
	fn ensure_successful_delivery(
		origin_ref: &Location,
		dest: &Location,
		fee_reason: xcm_executor::traits::FeeReason,
	) -> (Option<xcm_executor::FeesMode>, Option<Assets>) {
		use xcm::{latest::MAX_ITEMS_IN_ASSETS, MAX_INSTRUCTIONS_TO_DECODE};
		use xcm_executor::{
			traits::{FeeManager, TransactAsset},
			FeesMode,
		};

		// check if the destination is relay/parent
		if dest.ne(&Location::parent()) {
			return (None, None);
		}

		// Ensure routers
		XcmConfig::XcmSender::ensure_successful_delivery(Some(Location::parent()));

		let mut fees_mode = None;
		if !XcmConfig::FeeManager::is_waived(Some(origin_ref), fee_reason) {
			// if not waived, we need to set up accounts for paying and receiving fees
			let context = XcmContext { origin: None, message_id: XcmHash::default(), topic: None };

			// mint ED to origin if needed
			if let Some(ed) = ExistentialDeposit::get() {
				let holdings = XcmConfig::AssetTransactor::mint_asset(&ed, &context).unwrap();
				XcmConfig::AssetTransactor::deposit_asset(holdings, &origin_ref, Some(&context))
					.unwrap();
			}

			// overestimate delivery fee
			let mut max_assets: Vec<Asset> = Vec::new();
			for i in 0..MAX_ITEMS_IN_ASSETS {
				max_assets.push((GeneralIndex(i as u128), 100u128).into());
			}
			let overestimated_xcm =
				vec![WithdrawAsset(max_assets.into()); MAX_INSTRUCTIONS_TO_DECODE as usize].into();
			let overestimated_fees = PriceForDelivery::price_for_delivery((), &overestimated_xcm);

			// mint overestimated fee to origin
			for fee in overestimated_fees.inner() {
				let holdings = XcmConfig::AssetTransactor::mint_asset(fee, &context).unwrap();
				XcmConfig::AssetTransactor::deposit_asset(holdings, &origin_ref, Some(&context))
					.unwrap();
			}

			// expected worst case - direct withdraw
			fees_mode = Some(FeesMode { jit_withdraw: true });
		}
		(fees_mode, None)
	}
}
