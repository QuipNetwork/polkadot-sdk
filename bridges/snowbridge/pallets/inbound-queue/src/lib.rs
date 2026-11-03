// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>
//! Inbound Queue
//!
//! # Overview
//!
//! Receives messages emitted by the Gateway contract on Ethereum, whereupon they are verified,
//! translated to XCM, and finally sent to their final destination parachain.
//!
//! The message relayers are rewarded using native currency from the sovereign account of the
//! destination parachain.
//!
//! # Extrinsics
//!
//! ## Governance
//!
//! * [`Call::set_operating_mode`]: Set the operating mode of the pallet. Can be used to disable
//!   processing of inbound messages.
//!
//! ## Message Submission
//!
//! * [`Call::submit`]: Submit a message for verification and dispatch the final destination
//!   parachain.
#![cfg_attr(not(feature = "std"), no_std)]

mod envelope;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

pub mod weights;

#[cfg(test)]
mod mock;

#[cfg(test)]
mod test;

use codec::{Decode, DecodeAll, Encode};
use envelope::Envelope;
use frame_support::{
	traits::{
		fungible::{Inspect, Mutate},
		tokens::{Fortitude, Preservation},
	},
	weights::WeightToFee,
	PalletError,
};
use frame_system::ensure_signed;
use scale_info::TypeInfo;
use sp_core::H160;
use sp_runtime::traits::Zero;
use xcm::prelude::{
	send_xcm, Junction::*, Location, SendError as XcmpSendError, SendXcm, Xcm, XcmContext, XcmHash,
};
use xcm_executor::traits::TransactAsset;

use snowbridge_core::{
	sibling_sovereign_account, BasicOperatingMode, Channel, ChannelId, ParaId, PricingParameters,
	StaticLookup,
};
use snowbridge_inbound_queue_primitives::{
	v1::{ConvertMessage, ConvertMessageError, VersionedMessage},
	EventProof, VerificationError, Verifier,
};

use sp_runtime::{traits::Saturating, SaturatedConversion, TokenError};

pub use weights::WeightInfo;

type BalanceOf<T> =
	<<T as pallet::Config>::Token as Inspect<<T as frame_system::Config>::AccountId>>::Balance;

pub use pallet::*;

pub const LOG_TARGET: &str = "snowbridge-inbound-queue";

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_core::H256;

	#[cfg(feature = "runtime-benchmarks")]
	use snowbridge_inbound_queue_primitives::EventFixture;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[cfg(feature = "runtime-benchmarks")]
	pub trait BenchmarkHelper<T> {
		fn initialize_storage() -> EventFixture;
	}

	#[pallet::config]
	pub trait Config: frame_system::Config<AccountId: Into<[u8; 32]>> {
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The verifier for inbound messages from Ethereum
		type Verifier: Verifier;

		/// Message relayers are rewarded with this asset
		type Token: Mutate<Self::AccountId> + Inspect<Self::AccountId>;

		/// XCM message sender
		type XcmSender: SendXcm;

		// Address of the Gateway contract
		#[pallet::constant]
		type GatewayAddress: Get<H160>;

		/// Convert inbound message to XCM
		type MessageConverter: ConvertMessage<
			AccountId = Self::AccountId,
			Balance = BalanceOf<Self>,
		>;

		/// Lookup a channel descriptor
		type ChannelLookup: StaticLookup<Source = ChannelId, Target = Channel>;

		/// Lookup pricing parameters
		type PricingParameters: Get<PricingParameters<BalanceOf<Self>>>;

		type WeightInfo: WeightInfo;

		#[cfg(feature = "runtime-benchmarks")]
		type Helper: BenchmarkHelper<Self>;

		/// Convert a weight value into deductible balance type.
		type WeightToFee: WeightToFee<Balance = BalanceOf<Self>>;

		/// Convert a length value into deductible balance type
		type LengthToFee: WeightToFee<Balance = BalanceOf<Self>>;

		/// The upper limit here only used to estimate delivery cost
		type MaxMessageSize: Get<u32>;

		/// To withdraw and deposit an asset.
		type AssetTransactor: TransactAsset;
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A message was received from Ethereum
		MessageReceived {
			/// The message channel
			channel_id: ChannelId,
			/// The message nonce
			nonce: u64,
			/// ID of the XCM message which was forwarded to the final destination parachain
			message_id: [u8; 32],
			/// Fee burned for the teleport
			fee_burned: BalanceOf<T>,
		},
		/// Set OperatingMode
		OperatingModeChanged { mode: BasicOperatingMode },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Message came from an invalid outbound channel on the Ethereum side.
		InvalidGateway,
		/// Message has an invalid envelope.
		InvalidEnvelope,
		/// Message has an unexpected nonce.
		InvalidNonce,
		/// Message has an invalid payload.
		InvalidPayload,
		/// Message channel is invalid
		InvalidChannel,
		/// The max nonce for the type has been reached
		MaxNonceReached,
		/// Cannot convert location
		InvalidAccountConversion,
		/// Pallet is halted
		Halted,
		/// Message verification error,
		Verification(VerificationError),
		/// XCMP send failure
		Send(SendError),
		/// Message conversion error
		ConvertMessage(ConvertMessageError),
	}

	#[derive(
		Clone, Encode, Decode, DecodeWithMemTracking, Eq, PartialEq, Debug, TypeInfo, PalletError,
	)]
	pub enum SendError {
		NotApplicable,
		NotRoutable,
		Transport,
		DestinationUnsupported,
		ExceedsMaxMessageSize,
		MissingArgument,
		Fees,
	}

	impl<T: Config> From<XcmpSendError> for Error<T> {
		fn from(e: XcmpSendError) -> Self {
			match e {
				XcmpSendError::NotApplicable => Error::<T>::Send(SendError::NotApplicable),
				XcmpSendError::Unroutable => Error::<T>::Send(SendError::NotRoutable),
				XcmpSendError::Transport(_) => Error::<T>::Send(SendError::Transport),
				XcmpSendError::DestinationUnsupported => {
					Error::<T>::Send(SendError::DestinationUnsupported)
				},
				XcmpSendError::ExceedsMaxMessageSize => {
					Error::<T>::Send(SendError::ExceedsMaxMessageSize)
				},
				XcmpSendError::MissingArgument => Error::<T>::Send(SendError::MissingArgument),
				XcmpSendError::Fees => Error::<T>::Send(SendError::Fees),
			}
		}
	}

	/// The current nonce for each channel
	#[pallet::storage]
	pub type Nonce<T: Config> = StorageMap<_, Twox64Concat, ChannelId, u64, ValueQuery>;

	/// The current operating mode of the pallet.
	#[pallet::storage]
	#[pallet::getter(fn operating_mode)]
	pub type OperatingMode<T: Config> = StorageValue<_, BasicOperatingMode, ValueQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Submit an inbound message originating from the Gateway contract on Ethereum
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::submit())]
		pub fn submit(origin: OriginFor<T>, event: EventProof) -> DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(!Self::operating_mode().is_halted(), Error::<T>::Halted);

			// submit message to verifier for verification
			T::Verifier::verify(&event.event_log, &event.proof)
				.map_err(|e| Error::<T>::Verification(e))?;

			// Decode event log into an Envelope
			let envelope =
				Envelope::try_from(&event.event_log).map_err(|_| Error::<T>::InvalidEnvelope)?;

			// Verify that the message was submitted from the known Gateway contract
			ensure!(T::GatewayAddress::get() == envelope.gateway, Error::<T>::InvalidGateway);

			// Retrieve the registered channel for this message
			let channel =
				T::ChannelLookup::lookup(envelope.channel_id).ok_or(Error::<T>::InvalidChannel)?;

			// Verify message nonce
			<Nonce<T>>::try_mutate(envelope.channel_id, |nonce| -> DispatchResult {
				if *nonce == u64::MAX {
					return Err(Error::<T>::MaxNonceReached.into());
				}
				if envelope.nonce != nonce.saturating_add(1) {
					Err(Error::<T>::InvalidNonce.into())
				} else {
					*nonce = nonce.saturating_add(1);
					Ok(())
				}
			})?;

			// Decode payload into `VersionedMessage`
			let message = VersionedMessage::decode_all(&mut envelope.payload.as_ref())
				.map_err(|_| Error::<T>::InvalidPayload)?;

			// Decode message into XCM
			let (xcm, fee) = Self::do_convert(envelope.message_id, message.clone())?;

			tracing::info!(
				target: LOG_TARGET,
				?xcm,
				?fee,
				"💫 xcm decoded"
			);

			// Burning fees for teleport
			Self::burn_for_teleport(channel.para_id, &who, fee)?;

			// Reimburse the relayer from the sovereign account of the destination parachain, only
			// if funds are available. The relayer has already fronted the teleported fee.
			let sovereign_account = sibling_sovereign_account::<T>(channel.para_id);
			let delivery_cost = Self::calculate_delivery_cost(event.encode().len() as u32);
			let owed = fee.saturating_add(delivery_cost);
			let amount = T::Token::reducible_balance(
				&sovereign_account,
				Preservation::Preserve,
				Fortitude::Polite,
			)
			.min(owed);
			if !amount.is_zero() {
				// Best effort attempt to reward relayer if there are funds available.
				let _ =
					T::Token::transfer(&sovereign_account, &who, amount, Preservation::Preserve);
			}

			// Attempt to send XCM to a dest parachain
			let message_id = Self::send_xcm(xcm, channel.para_id)?;

			Self::deposit_event(Event::MessageReceived {
				channel_id: envelope.channel_id,
				nonce: envelope.nonce,
				message_id,
				fee_burned: fee,
			});

			Ok(())
		}

		/// Halt or resume all pallet operations. May only be called by root.
		#[pallet::call_index(1)]
		#[pallet::weight((T::DbWeight::get().reads_writes(1, 1), DispatchClass::Operational))]
		pub fn set_operating_mode(
			origin: OriginFor<T>,
			mode: BasicOperatingMode,
		) -> DispatchResult {
			ensure_root(origin)?;
			OperatingMode::<T>::set(mode);
			Self::deposit_event(Event::OperatingModeChanged { mode });
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		pub fn do_convert(
			message_id: H256,
			message: VersionedMessage,
		) -> Result<(Xcm<()>, BalanceOf<T>), Error<T>> {
			let (xcm, fee) = T::MessageConverter::convert(message_id, message)
				.map_err(|e| Error::<T>::ConvertMessage(e))?;
			Ok((xcm, fee))
		}

		pub fn send_xcm(xcm: Xcm<()>, dest: ParaId) -> Result<XcmHash, Error<T>> {
			let dest = Location::new(1, [Parachain(dest.into())]);
			let (xcm_hash, _) = send_xcm::<T::XcmSender>(dest, xcm).map_err(Error::<T>::from)?;
			Ok(xcm_hash)
		}

		pub fn calculate_delivery_cost(length: u32) -> BalanceOf<T> {
			let weight_fee = T::WeightToFee::weight_to_fee(&T::WeightInfo::submit());
			let len_fee = T::LengthToFee::weight_to_fee(&Weight::from_parts(length as u64, 0));
			weight_fee
				.saturating_add(len_fee)
				.saturating_add(T::PricingParameters::get().rewards.local)
		}

		/// Teleport the fee, fronted by the relayer, to the destination parachain.
		fn burn_for_teleport(
			dest: ParaId,
			who: &T::AccountId,
			amount: BalanceOf<T>,
		) -> DispatchResult {
			let dummy_context =
				XcmContext { origin: None, message_id: Default::default(), topic: None };
			let dest = Location::new(1, [Parachain(dest.into())]);
			let who = who.clone().into();
			let who = Location::new(0, [AccountId32 { network: None, id: who }]);
			let fees = (Location::parent(), amount.saturated_into::<u128>()).into();
			T::AssetTransactor::can_check_out(&dest, &fees, &dummy_context).map_err(|error| {
				tracing::error!(
					target: LOG_TARGET,
					?error,
					"XCM asset check out failed with error"
				);
				TokenError::FundsUnavailable
			})?;
			T::AssetTransactor::check_out(&dest, &fees, &dummy_context);
			T::AssetTransactor::withdraw_asset(&fees, &who, None).map_err(|error| {
				tracing::error!(
					target: LOG_TARGET,
					?error,
					"XCM asset withdraw failed with error"
				);
				TokenError::FundsUnavailable
			})?;
			Ok(())
		}
	}

	/// API for accessing the delivery cost of a message
	impl<T: Config> Get<BalanceOf<T>> for Pallet<T> {
		fn get() -> BalanceOf<T> {
			// Cost here based on MaxMessagePayloadSize(the worst case)
			Self::calculate_delivery_cost(T::MaxMessageSize::get())
		}
	}
}
