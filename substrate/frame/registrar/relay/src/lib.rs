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

//! # Relay-chain registrar pallet
//!
//! Relay half of the parachain registrar. Runs on the relay chain, applying registrations
//! authorized on a parachain (`pallet-registrar-para`) and driving the relay's legacy `paras`
//! state.
//!
//! ## Two-phase registration
//!
//! Pushing a multi-megabyte validation code through XCM would be wasteful when the parachain can
//! commit to the exact bytes and let anybody upload them here directly, so registration arrives in
//! two pieces:
//!
//! 1. [`Pallet::authorize_code`] takes the parachain's request, which carries the head data plus
//!    the hash and length of the code that is coming, and parks it in [`PendingRegistrations`].
//!    Only callable by a trusted XCM origin (e.g. the Coretime chain).
//! 2. [`Pallet::apply_authorized_code`] takes the blob itself. It needs no signature: anybody may
//!    push the code, because a pending entry already pins down exactly which bytes are acceptable,
//!    and the parachain has already made the manager pay for them. If the blob matches, the para is
//!    onboarded and the outcome is reported back to the parachain.
//!
//! An authorization does not time out here. If the code never turns up, missing the deadline is the
//! manager's problem, not this chain's: the parachain sends [`Pallet::cancel_authorization`] when
//! it gives up, this pallet drops the entry and confirms, and the parachain releases the deposit.
//! So no per-block sweep runs on the relay chain, and whoever wants the deposit back pays for the
//! round trip. No deposit is ever taken here.
//!
//! ## Runtime requirement
//!
//! `apply_authorized_code` authorizes itself through [`frame_support::pallet_macros::authorize`],
//! so the runtime must carry `frame_system::AuthorizeCall` in its transaction extension pipeline.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use frame_support::traits::Get;
use registrar_primitives::{
	FailureReason, MessageToPara, MessageToParaV1, MessageToRelay, MessageToRelayV1, Outcome,
	ParaId, ParachainRegistrar,
};
use scale_info::TypeInfo;
use sp_core::H256;

pub use pallet::*;
pub use weights::WeightInfo;

pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub trait SendToPara {
	/// Send `message` to the parachain.
	///
	/// `Err(())` means the transport refused the message. Callers here are mid-way through
	/// applying state that must survive, so they log and carry on rather than unwinding.
	#[allow(clippy::result_unit_err)]
	fn send(message: MessageToPara) -> Result<(), ()>;
}

#[cfg(feature = "std")]
impl SendToPara for () {
	fn send(_message: MessageToPara) -> Result<(), ()> {
		Ok(())
	}
}

/// A registration the parachain has asked for, waiting on its validation code.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, Eq, PartialEq, TypeInfo, MaxEncodedLen, Debug,
)]
#[scale_info(skip_type_params(MaxHeadDataSize))]
pub struct PendingRegistration<AccountId, MaxHeadDataSize: Get<u32>> {
	/// The id of the [`MessageToRelayV1::Register`] that created this entry, echoed back in the
	/// response once the code arrives.
	pub message_id: u64,
	/// The account managing this registration on the parachain.
	pub manager: AccountId,
	/// The genesis head data, held here until the code arrives.
	pub genesis_head: frame_support::BoundedVec<u8, MaxHeadDataSize>,
	/// Blake2-256 hash the validation code must have.
	pub code_hash: H256,
	/// Exact length the validation code must have.
	///
	/// The parachain sized the manager's deposit from this, so a blob of any other length would
	/// mean the manager underpaid, even if the hash somehow matched.
	pub code_len: u32,
}

/// How much head data a message carries, for weighing [`Pallet::authorize_code`].
///
/// Worth doing rather than always charging `MaxHeadDataSize`: that bound is a megabyte on
/// production relay chains, and a typical genesis head is nowhere near it.
pub fn head_data_len<AccountId>(message: &MessageToRelay<AccountId>) -> u32 {
	match message {
		MessageToRelay::V1(MessageToRelayV1::Register { genesis_head, .. }) => {
			genesis_head.len() as u32
		},
		MessageToRelay::V1(MessageToRelayV1::CancelRegistration { .. }) => 0,
	}
}

/// [`PendingRegistration`] as this pallet stores it.
pub type PendingRegistrationOf<T> =
	PendingRegistration<<T as frame_system::Config>::AccountId, <T as Config>::MaxHeadDataSize>;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::{BlakeTwo256, Hash};

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// A trusted parachain parachain authorized to drive registrations.
		type ParaOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Sends messages to the parachain.
		type SendToPara: SendToPara;

		/// The relay chain's parachain registry.
		type Registrar: ParachainRegistrar<AccountId = Self::AccountId>;

		/// The largest head data this pallet will hold onto while waiting for code.
		///
		/// Should be at least the relay chain's `max_head_data_size`.
		#[pallet::constant]
		type MaxHeadDataSize: Get<u32>;

		/// The largest validation code [`Pallet::apply_authorized_code`] will accept.
		///
		/// Should be at least the relay chain's `max_code_size`.
		#[pallet::constant]
		type MaxCodeSize: Get<u32>;

		/// How many registrations may be waiting on their code at once.
		///
		/// Bounds the head data this pallet stores while no deposit is held here. Entries only ever
		/// leave by the code landing or by the parachain cancelling, so a manager who does neither
		/// occupies a slot for as long as they keep paying the deposit on the parachain.
		#[pallet::constant]
		type MaxPendingRegistrations: Get<u32>;

		/// Priority given to a valid [`Pallet::apply_authorized_code`] in the transaction pool.
		#[pallet::constant]
		type UnsignedPriority: Get<TransactionPriority>;

		/// Weight information for the extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Registrations waiting on their validation code, by para id.
	///
	/// Counted so [`Config::MaxPendingRegistrations`] can be enforced with a single read.
	#[pallet::storage]
	pub type PendingRegistrations<T: Config> =
		CountedStorageMap<_, Blake2_128Concat, ParaId, PendingRegistrationOf<T>>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A registration request was accepted and is waiting on its validation code.
		RegistrationPending { para_id: ParaId, message_id: u64, code_hash: H256 },
		/// A registration request was rejected out of hand.
		RegistrationRejected { para_id: ParaId, message_id: u64, reason: FailureReason },
		/// A para was onboarded.
		Registered { para_id: ParaId, message_id: u64, manager: T::AccountId },
		/// An authorization was dropped at the parachain's request.
		AuthorizationCancelled { para_id: ParaId, message_id: u64 },
		/// A cancellation arrived after the para had already been onboarded, and was refused.
		CancellationRefused { para_id: ParaId, message_id: u64 },
		/// A report could not be sent back to the parachain.
		///
		/// The relay chain's own state is already correct; the parachain is now out of step and
		/// will need its manager to ask again.
		ReportFailed { para_id: ParaId, message_id: u64 },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// No registration is waiting on code for this para id.
		NothingPending,
		/// The validation code does not match the hash the parachain committed to.
		CodeHashMismatch,
		/// The validation code is not the length the parachain committed to.
		CodeLenMismatch,
		/// The validation code is larger than this pallet will accept.
		CodeTooLarge,
		/// The message is not one this call serves.
		UnexpectedMessage,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Accept a control-plane message from the parachain's registrar pallet and authorize the
		/// validation code that will follow.
		///
		/// Only callable by a trusted XCM origin (e.g. the Coretime chain), never by users.
		///
		/// A request this pallet will not act on is *not* an extrinsic failure. Failing would roll
		/// back the rejection report along with everything else, and the parachain would sit on a
		/// held deposit waiting for news that never comes. So a rejection is applied, reported, and
		/// returns `Ok`.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::authorize_code(head_data_len(message)))]
		pub fn authorize_code(
			origin: OriginFor<T>,
			message: MessageToRelay<T::AccountId>,
		) -> DispatchResult {
			T::ParaOrigin::ensure_origin_or_root(origin)?;

			match message {
				MessageToRelay::V1(MessageToRelayV1::Register {
					para_id,
					message_id,
					manager,
					genesis_head,
					code_hash,
					code_len,
				}) => {
					Self::on_register_request(
						para_id,
						message_id,
						manager,
						genesis_head,
						code_hash,
						code_len,
					);
					Ok(())
				},
				_ => Err(Error::<T>::UnexpectedMessage.into()),
			}
		}

		/// Upload the validation code for a pending authorization, onboarding the para.
		///
		/// Needs no signature and pays no fee. Anybody may submit: the pending entry already fixes
		/// the exact bytes that will be accepted, and the manager has already paid for them on the
		/// parachain.
		#[pallet::call_index(1)]
		#[pallet::authorize(Self::authorize_apply_authorized_code)]
		#[pallet::weight_of_authorize(T::WeightInfo::authorize_apply_authorized_code(validation_code.len() as u32))]
		#[pallet::weight(T::WeightInfo::apply_authorized_code(validation_code.len() as u32))]
		pub fn apply_authorized_code(
			origin: OriginFor<T>,
			para_id: ParaId,
			validation_code: Vec<u8>,
		) -> DispatchResultWithPostInfo {
			ensure_authorized(origin)?;

			let pending = Self::validate_pending_code(para_id, &validation_code)?;

			T::Registrar::register(
				pending.manager.clone(),
				para_id,
				pending.genesis_head.into_inner(),
				validation_code,
			)?;
			PendingRegistrations::<T>::remove(para_id);

			let message_id = pending.message_id;
			Self::report_registration(para_id, message_id, Ok(()));
			Self::deposit_event(Event::Registered {
				para_id,
				message_id,
				manager: pending.manager,
			});
			Ok(Pays::No.into())
		}

		/// Drop the authorization held for a para id, at the parachain's request.
		///
		/// Only callable by a trusted XCM origin (e.g. the Coretime chain), never by users. This is
		/// the only way an authorization that never received its code goes away, and the manager
		/// pays for it on the parachain: nothing here expires on its own.
		///
		/// Answered with [`MessageToParaV1::CancelResponse`], which is what lets the parachain
		/// release the deposit. Refused, and reported as such, if the code did land in the meantime
		/// and the para is registered: the deposit is then owed after all.
		///
		/// Cancelling something that was never pending is not an error. The request may simply have
		/// been rejected here and the report lost, and the parachain still needs an answer it can
		/// act on.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::cancel_authorization())]
		pub fn cancel_authorization(
			origin: OriginFor<T>,
			message: MessageToRelay<T::AccountId>,
		) -> DispatchResult {
			T::ParaOrigin::ensure_origin_or_root(origin)?;

			match message {
				MessageToRelay::V1(MessageToRelayV1::CancelRegistration {
					para_id,
					message_id,
				}) => {
					Self::on_cancel_request(para_id, message_id);
					Ok(())
				},
				_ => Err(Error::<T>::UnexpectedMessage.into()),
			}
		}
	}

	impl<T: Config> Pallet<T> {
		/// Decide whether an unsigned [`Pallet::apply_authorized_code`] may enter the pool and a
		/// block.
		///
		/// Runs exactly the same checks as the dispatch, so the pool and the block never disagree
		/// about which bytes are acceptable.
		// `#[pallet::authorize]` hands the call arguments over by reference, so the parameter
		// types have to mirror the call's exactly. `&[u8]` would not compile.
		#[allow(clippy::ptr_arg)]
		pub fn authorize_apply_authorized_code(
			_source: TransactionSource,
			para_id: &ParaId,
			validation_code: &Vec<u8>,
		) -> TransactionValidityWithRefund {
			let pending = Self::validate_pending_code(*para_id, validation_code)
				.map_err(|e| InvalidTransaction::Custom(Self::err_to_code(e)))?;

			// No longevity bound: an authorization does not expire, so the transaction stays valid
			// until the code is applied or the parachain cancels, and revalidation drops it then.
			let validity = ValidTransaction::with_tag_prefix("RegistrarApplyAuthorizedCode")
				.priority(T::UnsignedPriority::get())
				.and_provides((*para_id, pending.code_hash))
				.propagate(true)
				.build()?;

			Ok((validity, Weight::zero()))
		}

		/// Apply a registration request from the parachain, accepting or rejecting it.
		fn on_register_request(
			para_id: ParaId,
			message_id: u64,
			manager: T::AccountId,
			genesis_head: Vec<u8>,
			code_hash: H256,
			code_len: u32,
		) {
			let Ok(head_len) = u32::try_from(genesis_head.len()) else {
				return Self::reject(para_id, message_id, FailureReason::InvalidOnboardingData);
			};

			if T::Registrar::is_registered(para_id) ||
				PendingRegistrations::<T>::contains_key(para_id)
			{
				return Self::reject(para_id, message_id, FailureReason::AlreadyRegistered);
			}
			if PendingRegistrations::<T>::count() >= T::MaxPendingRegistrations::get() {
				return Self::reject(para_id, message_id, FailureReason::TooManyPending);
			}
			if code_len > T::MaxCodeSize::get() ||
				T::Registrar::check_onboarding(head_len, code_len).is_err()
			{
				return Self::reject(para_id, message_id, FailureReason::InvalidOnboardingData);
			}
			let Ok(genesis_head) = BoundedVec::try_from(genesis_head) else {
				return Self::reject(para_id, message_id, FailureReason::InvalidOnboardingData);
			};

			PendingRegistrations::<T>::insert(
				para_id,
				PendingRegistration { message_id, manager, genesis_head, code_hash, code_len },
			);

			Self::deposit_event(Event::RegistrationPending { para_id, message_id, code_hash });
		}

		/// Turn a request away and tell the parachain to release the deposit.
		fn reject(para_id: ParaId, message_id: u64, reason: FailureReason) {
			Self::report_registration(para_id, message_id, Err(reason.clone()));
			Self::deposit_event(Event::RegistrationRejected { para_id, message_id, reason });
		}

		/// Drop the authorization for `para_id`, unless the code beat the cancellation here.
		///
		/// The relay chain is the authority on which of the two happened first, which is what makes
		/// it safe for the parachain to release a deposit on the strength of this answer. A para id
		/// this chain has registered is not one whose deposit can be handed back, so that is the
		/// whole test. The entry goes either way: once the id is taken, an authorization for it can
		/// never be applied.
		fn on_cancel_request(para_id: ParaId, message_id: u64) {
			PendingRegistrations::<T>::remove(para_id);

			if T::Registrar::is_registered(para_id) {
				Self::report_cancellation(
					para_id,
					message_id,
					Err(FailureReason::AlreadyRegistered),
				);
				return Self::deposit_event(Event::CancellationRefused { para_id, message_id });
			}

			Self::report_cancellation(para_id, message_id, Ok(()));
			Self::deposit_event(Event::AuthorizationCancelled { para_id, message_id });
		}

		/// Check `validation_code` against the pending entry for `para_id`.
		fn validate_pending_code(
			para_id: ParaId,
			validation_code: &[u8],
		) -> Result<PendingRegistrationOf<T>, Error<T>> {
			// Bound the work before hashing, so an oversized blob is rejected cheaply.
			let code_len =
				u32::try_from(validation_code.len()).map_err(|_| Error::<T>::CodeTooLarge)?;
			ensure!(code_len <= T::MaxCodeSize::get(), Error::<T>::CodeTooLarge);

			let pending =
				PendingRegistrations::<T>::get(para_id).ok_or(Error::<T>::NothingPending)?;
			ensure!(code_len == pending.code_len, Error::<T>::CodeLenMismatch);
			ensure!(
				BlakeTwo256::hash(validation_code) == pending.code_hash,
				Error::<T>::CodeHashMismatch
			);

			Ok(pending)
		}

		/// Map a validation failure onto the `InvalidTransaction::Custom` code it reports.
		pub fn err_to_code(error: Error<T>) -> u8 {
			match error {
				Error::<T>::NothingPending => 0,
				Error::<T>::CodeHashMismatch => 1,
				Error::<T>::CodeLenMismatch => 2,
				Error::<T>::CodeTooLarge => 3,
				Error::<T>::UnexpectedMessage => 4,
			}
		}

		/// Tell the parachain how a registration ended.
		fn report_registration(para_id: ParaId, message_id: u64, outcome: Outcome) {
			Self::report(
				para_id,
				message_id,
				MessageToParaV1::RegisterResponse { para_id, message_id, outcome },
			);
		}

		/// Tell the parachain what became of its cancellation.
		fn report_cancellation(para_id: ParaId, message_id: u64, outcome: Outcome) {
			Self::report(
				para_id,
				message_id,
				MessageToParaV1::CancelResponse { para_id, message_id, outcome },
			);
		}

		/// Hand a report to the transport.
		///
		/// A transport failure is only logged and surfaced as an event: every caller has already
		/// committed relay-chain state that must not be unwound just because the report bounced.
		fn report(para_id: ParaId, message_id: u64, message: MessageToParaV1) {
			if T::SendToPara::send(MessageToPara::V1(message)).is_err() {
				log::error!(
					target: "runtime::registrar-relay",
					"failed to report the outcome for para {para_id} back to the parachain",
				);
				Self::deposit_event(Event::ReportFailed { para_id, message_id });
			}
		}
	}
}
