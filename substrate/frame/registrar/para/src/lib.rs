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

//! # Parachain registrar pallet
//!
//! The user-facing half of parachain registration. It hands out para ids, takes the manager's
//! deposits as [`Consideration`] tickets, and coordinates the registration itself asynchronously
//! with the chain that owns the parachain registry.
//!
//! Both directions of that coordination are abstract: requests go out through [`SendToRelay`],
//! verdicts come back in through [`Pallet::receive`], gated by [`Config::RelayOrigin`]. Nothing
//! here depends on XCM or on the other chain's extrinsics.
//!
//! ## Registration flow
//!
//! Registration takes two transactions on two chains. Sending a multi-megabyte validation code
//! through the messaging layer would be wasteful, so this chain only commits to its hash and
//! length and the blob is uploaded to the relay chain directly:
//!
//! 1. [`Pallet::reserve`] allocates a para id here and takes [`Config::ReservationConsideration`].
//! 2. [`Pallet::register`] takes [`Config::RegistrationConsideration`] for the head data and the
//!    *declared* code length, then asks the relay chain to accept the registration. Only the code
//!    hash and length are sent.
//! 3. The manager uploads the validation code to the relay chain, which accepts it only if it
//!    matches the hash and length committed to in step 2.
//! 4. The verdict arrives back as [`Pallet::receive`], which either finalises the registration or
//!    releases the registration deposit.
//!
//! ## Giving up
//!
//! Nothing on the relay chain times a registration out, so a request whose code never turns up
//! waits until the manager ends it with [`Pallet::cancel_registration`]. That asks the relay chain
//! to drop the authorization and only releases the deposit once it confirms, which is what
//! keeps a cancellation from freeing the deposit on a para that did register after all.
//!
//! Deposits only ever live on this chain; the relay chain takes nothing.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::{
	defensive,
	traits::{Consideration, Footprint},
};
use registrar_primitives::{
	FailureReason, MessageToPara, MessageToParaV1, MessageToRelay, MessageToRelayV1, Outcome,
	ParaId,
};
use scale_info::TypeInfo;
use sp_core::H256;
use sp_runtime::{
	traits::{BlockNumberProvider, Saturating},
	DispatchResult,
};

pub use pallet::*;
pub use weights::WeightInfo;

pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

/// Block number used for registration deadlines.
///
/// On a parachain, configure [`Config::BlockNumberProvider`] to
/// `cumulus_pallet_parachain_system::RelaychainDataProvider`, so deadlines are expressed in
/// relay-chain blocks and keep their meaning through a stall in this chain's own block production.
pub type ProvidedBlockNumberOf<T> =
	<<T as Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

/// Used to send an XCM `Transact` to the registrar pallet on the remote relay chain.
pub trait SendToRelay {
	/// The account id used to identify a registration's manager on both chains.
	type AccountId;

	/// Send `message` to the relay chain.
	///
	/// `Err(())` means the message could not be handed to the transport at all. Callers are
	/// expected to fail the whole extrinsic, so nothing is left half-done.
	#[allow(clippy::result_unit_err)]
	fn send(message: MessageToRelay<Self::AccountId>) -> Result<(), ()>;
}

#[cfg(feature = "std")]
impl SendToRelay for () {
	type AccountId = sp_runtime::AccountId32;

	fn send(_message: MessageToRelay<Self::AccountId>) -> Result<(), ()> {
		Ok(())
	}
}

/// Where a para id sits in the registration flow.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq, Debug, TypeInfo, MaxEncodedLen,
)]
pub enum RegistrationState<Ticket, BlockNumber> {
	/// The para id is held by its manager, but nothing is registered on the relay chain yet.
	Reserved,
	/// The relay chain has been asked to register this para and has not reported back.
	Pending {
		/// The registration's [`Consideration`] ticket, returned if the registration fails.
		ticket: Ticket,
		/// The block from which the manager may give up on this registration.
		///
		/// Expressed in [`Config::BlockNumberProvider`] blocks. Long enough that a verdict already
		/// on its way arrives first, so a cancellation is only ever sent for a registration that
		/// really has gone quiet. Pushed out again by every [`Pallet::cancel_registration`], so a
		/// cancellation that gets lost can be retried but not spammed.
		cancellable_at: BlockNumber,
	},
	/// The relay chain has onboarded this para.
	Registered {
		/// The registration's [`Consideration`] ticket, kept while the para is registered.
		ticket: Ticket,
	},
}

/// Everything this chain knows about one para id.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq, TypeInfo, MaxEncodedLen, Debug,
)]
pub struct ParaInfo<AccountId, ReservationTicket, RegistrationTicket, BlockNumber> {
	/// The account that reserved the para id and controls it.
	pub manager: AccountId,
	/// The [`Consideration`] ticket for the para id itself.
	pub reservation: ReservationTicket,
	/// Where this para id sits in the registration flow.
	pub state: RegistrationState<RegistrationTicket, BlockNumber>,
}

/// The [`ParaInfo`] type as configured.
pub type ParaInfoOf<T> = ParaInfo<
	<T as frame_system::Config>::AccountId,
	<T as Config>::ReservationConsideration,
	<T as Config>::RegistrationConsideration,
	ProvidedBlockNumberOf<T>,
>;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::{DispatchResult, *};
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The cost of reserving a para id. The footprint is a single zero-sized item, so a flat
		/// price fits.
		type ReservationConsideration: Consideration<Self::AccountId, Footprint>;

		/// The cost of a registration, on top of the reservation. The footprint is one item sized
		/// as head data plus *declared* code length, so a per-byte price fits.
		type RegistrationConsideration: Consideration<Self::AccountId, Footprint>;

		/// Sends messages to the relay chain.
		type SendToRelay: SendToRelay<AccountId = Self::AccountId>;

		/// An origin that is sure to be the relay chain's registrar pallet.
		type RelayOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The lowest para id this pallet will hand out.
		///
		/// Mirrors the relay chain's `LOWEST_PUBLIC_ID`. Ids below it are reserved for system
		/// parachains and are not obtainable here.
		#[pallet::constant]
		type FirstPublicParaId: Get<ParaId>;

		/// The smallest validation code the relay chain will accept.
		///
		/// A local mirror of the relay chain's `MIN_CODE_SIZE`, used to fail early. The relay
		/// chain checks the real thing against its own live configuration.
		#[pallet::constant]
		type MinCodeSize: Get<u32>;

		/// The largest validation code the relay chain will accept.
		///
		/// A local mirror of the relay chain's `max_code_size`. See [`Config::MinCodeSize`].
		#[pallet::constant]
		type MaxCodeSize: Get<u32>;

		/// The largest head data the relay chain will accept.
		///
		/// A local mirror of the relay chain's `max_head_data_size`. See [`Config::MinCodeSize`].
		#[pallet::constant]
		type MaxHeadDataSize: Get<u32>;

		/// How long a manager waits for the relay chain before giving up on a registration.
		///
		/// Measured in [`Config::BlockNumberProvider`] blocks. Should comfortably cover a round
		/// trip, so that a verdict that is merely slow lands before anybody tries to cancel.
		#[pallet::constant]
		type PendingDeadline: Get<ProvidedBlockNumberOf<Self>>;

		/// Source of block numbers for registration deadlines.
		///
		/// On a parachain this should be
		/// `cumulus_pallet_parachain_system::RelaychainDataProvider`, so
		/// [`Config::PendingDeadline`] is in relay-chain blocks.
		type BlockNumberProvider: BlockNumberProvider;

		/// Weight information for the extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Hold reasons for runtimes that pay the considerations out of held funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Held for keeping a para id reserved.
		#[codec(index = 0)]
		ParaIdReservation,
		/// Held for the head data and validation code of a registration.
		#[codec(index = 1)]
		Registration,
	}

	/// The next para id that [`Pallet::reserve`] will hand out.
	#[pallet::storage]
	pub type NextFreeParaId<T: Config> = StorageValue<_, ParaId, ValueQuery>;

	/// The id the next message to the relay chain will carry.
	///
	/// One per message sent, echoed back in the relay chain's response, so a request, its
	/// response and the events on both chains can be tied together.
	#[pallet::storage]
	pub type NextMessageId<T: Config> = StorageValue<_, u64, ValueQuery>;

	/// Every para id reserved through this pallet, and what is happening with it.
	#[pallet::storage]
	pub type Paras<T: Config> = StorageMap<_, Blake2_128Concat, ParaId, ParaInfoOf<T>>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A para id was reserved.
		Reserved { para_id: ParaId, who: T::AccountId },
		/// A registration was requested and the relay chain has been asked to accept it.
		RegisterRequested { para_id: ParaId, message_id: u64, manager: T::AccountId },
		/// The relay chain confirmed a registration.
		Registered { para_id: ParaId, message_id: u64, manager: T::AccountId },
		/// The relay chain rejected a registration. The registration consideration was returned.
		RegistrationFailed {
			para_id: ParaId,
			message_id: u64,
			manager: T::AccountId,
			reason: FailureReason,
		},
		/// A manager gave up on a pending registration, and the relay chain has been asked to
		/// drop the authorization. The consideration stays taken until it answers.
		CancelRequested { para_id: ParaId, message_id: u64, manager: T::AccountId },
		/// The relay chain confirmed a cancellation. The registration consideration was returned.
		RegistrationCancelled { para_id: ParaId, message_id: u64, manager: T::AccountId },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The para id has not been reserved.
		NotReserved,
		/// The caller does not manage this para id.
		NotOwner,
		/// The para id is already registered, or a registration is already in flight for it.
		AlreadyRegistered,
		/// There is no registration in flight for this para id.
		NotPending,
		/// The manager may not abandon this registration yet.
		CannotCancelYet,
		/// The head data is larger than the relay chain will accept.
		HeadDataTooLarge,
		/// The validation code is larger than the relay chain will accept.
		CodeTooLarge,
		/// The validation code is smaller than the relay chain will accept.
		CodeTooSmall,
		/// The message could not be handed to the transport.
		SendFailed,
		/// There are no more para ids to hand out.
		NoFreeParaId,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			// Otherwise no validation code could ever pass `register`.
			assert!(
				T::MinCodeSize::get() <= T::MaxCodeSize::get(),
				"MinCodeSize ({}) must not exceed MaxCodeSize ({})",
				T::MinCodeSize::get(),
				T::MaxCodeSize::get(),
			);
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Reserve the next free para id for the caller.
		///
		/// Takes [`Config::ReservationConsideration`]. The caller becomes the manager of the new
		/// id and is the only account that may [`Pallet::register`] against it.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::reserve())]
		pub fn reserve(origin: OriginFor<T>) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let para_id = NextFreeParaId::<T>::get().max(T::FirstPublicParaId::get());
			let next = para_id.checked_add(1).ok_or(Error::<T>::NoFreeParaId)?;
			ensure!(!Paras::<T>::contains_key(para_id), Error::<T>::AlreadyRegistered);

			let reservation = T::ReservationConsideration::new(&who, Footprint::from_parts(1, 0))?;

			Paras::<T>::insert(
				para_id,
				ParaInfo { manager: who.clone(), reservation, state: RegistrationState::Reserved },
			);
			NextFreeParaId::<T>::put(next);

			Self::deposit_event(Event::Reserved { para_id, who });
			Ok(())
		}

		/// Ask the relay chain to register head data and validation code for a reserved para id.
		///
		/// The validation code itself stays here: only `code_hash` and `code_len` are sent. The
		/// caller uploads the blob to the relay chain separately, which accepts it only if it
		/// hashes to `code_hash` and is exactly `code_len` bytes long.
		///
		/// ## Costs
		///
		/// Takes [`Config::RegistrationConsideration`] for the head data and the *declared* code
		/// length, on top of the para id reservation. It is returned if the relay chain rejects
		/// the registration or if the caller later abandons it.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::register(genesis_head.len() as u32))]
		pub fn register(
			origin: OriginFor<T>,
			para_id: ParaId,
			genesis_head: Vec<u8>,
			code_len: u32,
			code_hash: H256,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let mut info = Paras::<T>::get(para_id).ok_or(Error::<T>::NotReserved)?;
			ensure!(info.manager == who, Error::<T>::NotOwner);
			ensure!(
				matches!(info.state, RegistrationState::Reserved),
				Error::<T>::AlreadyRegistered
			);

			let head_len = genesis_head.len() as u32;
			ensure!(head_len <= T::MaxHeadDataSize::get(), Error::<T>::HeadDataTooLarge);
			ensure!(code_len >= T::MinCodeSize::get(), Error::<T>::CodeTooSmall);
			ensure!(code_len <= T::MaxCodeSize::get(), Error::<T>::CodeTooLarge);

			let ticket = T::RegistrationConsideration::new(
				&who,
				Self::registration_footprint(head_len, code_len),
			)?;

			let cancellable_at = T::BlockNumberProvider::current_block_number()
				.saturating_add(T::PendingDeadline::get());
			info.state = RegistrationState::Pending { ticket, cancellable_at };
			Paras::<T>::insert(para_id, info);

			// A transport failure returns `Err` and unwinds everything above, ticket included.
			let message_id = Self::next_message_id();
			T::SendToRelay::send(MessageToRelay::V1(MessageToRelayV1::Register {
				para_id,
				message_id,
				manager: who.clone(),
				genesis_head,
				code_hash,
				code_len,
			}))
			.map_err(|()| Error::<T>::SendFailed)?;

			Self::deposit_event(Event::RegisterRequested { para_id, message_id, manager: who });
			Ok(())
		}

		/// Give up on a registration the relay chain never reported on.
		///
		/// Callable from [`Config::PendingDeadline`] blocks after the request. Nothing on the
		/// relay chain abandons a registration on its own, so this is what ends one whose code
		/// never turned up, and the manager pays for the round trip rather than every relay-chain
		/// block paying for a sweep.
		///
		/// The deposit is not released here: the relay chain is asked to drop the authorization
		/// first, and [`Pallet::receive`] releases the deposit when it confirms. Waiting for that
		/// answer is the point. A registration that did go through, with a verdict that got lost on
		/// the way here, must not have its deposit refunded, and only the relay chain knows
		/// which of the two happened.
		///
		/// The para id itself stays reserved either way, so the manager can simply try again.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::cancel_registration())]
		pub fn cancel_registration(origin: OriginFor<T>, para_id: ParaId) -> DispatchResult {
			let who = ensure_signed(origin)?;

			let mut info = Paras::<T>::get(para_id).ok_or(Error::<T>::NotReserved)?;
			ensure!(info.manager == who, Error::<T>::NotOwner);
			let RegistrationState::Pending { ticket, cancellable_at } = info.state else {
				return Err(Error::<T>::NotPending.into());
			};
			let now = T::BlockNumberProvider::current_block_number();
			ensure!(now >= cancellable_at, Error::<T>::CannotCancelYet);

			// Another deadline's grace before the manager may ask again, so a request that goes
			// missing can be retried without the relay chain being asked once per block.
			info.state = RegistrationState::Pending {
				ticket,
				cancellable_at: now.saturating_add(T::PendingDeadline::get()),
			};
			Paras::<T>::insert(para_id, info);

			// A transport failure returns `Err` and unwinds the new deadline with it.
			let message_id = Self::next_message_id();
			T::SendToRelay::send(MessageToRelay::V1(MessageToRelayV1::CancelRegistration {
				para_id,
				message_id,
			}))
			.map_err(|()| Error::<T>::SendFailed)?;

			Self::deposit_event(Event::CancelRequested { para_id, message_id, manager: who });
			Ok(())
		}

		/// Accept a report from the relay chain's registrar pallet.
		///
		/// Not callable by users: the origin must be the relay chain.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::receive())]
		pub fn receive(origin: OriginFor<T>, message: MessageToPara) -> DispatchResult {
			T::RelayOrigin::ensure_origin_or_root(origin)?;

			match message {
				MessageToPara::V1(MessageToParaV1::RegisterResponse {
					para_id,
					message_id,
					outcome,
				}) => Self::on_register_response(para_id, message_id, outcome),
				MessageToPara::V1(MessageToParaV1::CancelResponse {
					para_id,
					message_id,
					outcome,
				}) => Self::on_cancel_response(para_id, message_id, outcome),
			}
		}
	}
}

impl<T: Config> Pallet<T> {
	/// The footprint a registration is charged for: the head data plus the *declared* code length.
	pub fn registration_footprint(head_len: u32, code_len: u32) -> Footprint {
		Footprint::from_parts(1, head_len.saturating_add(code_len) as usize)
	}

	/// Take the id for the next message to the relay chain.
	fn next_message_id() -> u64 {
		NextMessageId::<T>::mutate(|next| {
			let id = *next;
			*next = next.wrapping_add(1);
			id
		})
	}

	/// Apply the relay chain's verdict on a registration.
	///
	/// A response about a para id we are not expecting one for is dropped rather than treated as a
	/// dispatch error: erroring here would unwind the whole incoming message for something we can
	/// do nothing about anyway. Unexpected responses still trip a defensive failure so they are
	/// loud in logs (and panic under `debug_assertions`).
	fn on_register_response(para_id: ParaId, message_id: u64, outcome: Outcome) -> DispatchResult {
		let Some(mut info) = Paras::<T>::get(para_id) else {
			defensive!("register response for unknown para, dropping", para_id);
			return Ok(());
		};
		let RegistrationState::Pending { ticket, .. } = info.state else {
			defensive!("register response for para which is not pending, dropping", para_id);
			return Ok(());
		};

		let manager = info.manager.clone();
		match outcome {
			Ok(()) => {
				info.state = RegistrationState::Registered { ticket };
				Paras::<T>::insert(para_id, info);
				Self::deposit_event(Event::Registered { para_id, message_id, manager });
			},
			Err(reason) => {
				ticket.drop(&info.manager)?;
				info.state = RegistrationState::Reserved;
				Paras::<T>::insert(para_id, info);
				Self::deposit_event(Event::RegistrationFailed {
					para_id,
					message_id,
					manager,
					reason,
				});
			},
		}

		Ok(())
	}

	/// Apply the relay chain's answer to a cancellation.
	///
	/// `Ok(())` means the authorization is gone, so the deposit goes back. The one refusal is
	/// [`FailureReason::AlreadyRegistered`]: the code landed after all and the earlier verdict was
	/// simply lost, so the para is recorded as registered and the deposit stays held.
	///
	/// Unlike a register response, an answer for a para that is no longer pending is expected
	/// rather than defensive: a verdict already in flight when the cancellation was sent settles
	/// the registration first, and this then has nothing left to do.
	fn on_cancel_response(para_id: ParaId, message_id: u64, outcome: Outcome) -> DispatchResult {
		let Some(mut info) = Paras::<T>::get(para_id) else {
			defensive!("cancel response for unknown para, dropping", para_id);
			return Ok(());
		};
		let RegistrationState::Pending { ticket, .. } = info.state else {
			log::debug!(
				target: "runtime::registrar-para",
				"cancel response for para {para_id} which is no longer pending, dropping",
			);
			return Ok(());
		};

		let manager = info.manager.clone();
		match outcome {
			Ok(()) => {
				ticket.drop(&info.manager)?;
				info.state = RegistrationState::Reserved;
				Paras::<T>::insert(para_id, info);
				Self::deposit_event(Event::RegistrationCancelled { para_id, message_id, manager });
			},
			Err(FailureReason::AlreadyRegistered) => {
				info.state = RegistrationState::Registered { ticket };
				Paras::<T>::insert(para_id, info);
				Self::deposit_event(Event::Registered { para_id, message_id, manager });
			},
			// Nothing else is a cancellation the relay chain refuses, so leave the registration
			// pending: the manager can ask again once the deadline comes round.
			Err(reason) => {
				defensive!("unexpected cancel refusal, leaving pending", (para_id, &reason));
			},
		}

		Ok(())
	}
}
