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

//! # Whitelist Pallet
//!
//! - [`Config`]
//! - [`Call`]
//!
//! ## Overview
//!
//! Allow some configurable origin: [`Config::WhitelistOrigin`] to whitelist some hash of a call,
//! and allow another configurable origin: [`Config::DispatchWhitelistedOrigin`] to dispatch them
//! with the root origin.
//!
//! In the meantime the call corresponding to the hash must have been submitted to the pre-image
//! handler [`pallet::Config::Preimages`].

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
pub mod weights;
pub use weights::WeightInfo;

extern crate alloc;

use alloc::boxed::Box;
use codec::{DecodeLimit, Encode, FullCodec};
use frame::{
	prelude::*,
	traits::{QueryPreimage, StorePreimage},
};
use scale_info::TypeInfo;

pub use pallet::*;

/// The Block number that we use to measure time.
///
/// Deferral expirations are tracked against this provider rather than the local system block,
/// so on a parachain it can be the relay chain block number. All `DeferredDispatch` `expire_at`
/// values and the [`Config::DeferredDispatchExpiration`] window are denominated in it.
pub type ProvidedBlockNumberFor<T> =
	<<T as Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

#[frame::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching call type.
		type RuntimeCall: IsType<<Self as frame_system::Config>::RuntimeCall>
			+ Dispatchable<RuntimeOrigin = Self::RuntimeOrigin, PostInfo = PostDispatchInfo>
			+ GetDispatchInfo
			+ FullCodec
			+ TypeInfo
			+ From<frame_system::Call<Self>>
			+ Parameter;

		/// Required origin for whitelisting a call.
		type WhitelistOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Required origin for dispatching whitelisted call with root origin.
		type DispatchWhitelistedOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The handler of pre-images.
		type Preimages: QueryPreimage<H = Self::Hashing> + StorePreimage;

		/// The number of provided blocks after which a deferred dispatch expires.
		type DeferredDispatchExpiration: Get<ProvidedBlockNumberFor<Self>>;

		/// Provider for the block number.
		type BlockNumberProvider: BlockNumberProvider;

		/// The weight information for this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		CallWhitelisted {
			call_hash: T::Hash,
		},
		WhitelistedCallRemoved {
			call_hash: T::Hash,
		},
		WhitelistedCallDispatched {
			call_hash: T::Hash,
			result: DispatchResultWithPostInfo,
		},
		/// A call dispatch has been deferred to a future provided block.
		DispatchDeferred {
			call_hash: T::Hash,
		},
		/// A deferred dispatch entry has been removed after expiration.
		DeferredDispatchRemoved {
			call_hash: T::Hash,
		},
		/// A relayer (signed origin) executed a deferred dispatch.
		///
		/// Emitted whenever the deferred entry is consumed by a relayer, regardless of whether the
		/// inner call itself succeeded; the inner call's outcome is reported separately by
		/// [`Event::WhitelistedCallDispatched`].
		DeferredDispatchExecuted {
			call_hash: T::Hash,
			who: T::AccountId,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The preimage of the call hash could not be loaded.
		UnavailablePreImage,
		/// The call could not be decoded.
		UndecodableCall,
		/// The weight of the decoded call was higher than the witness.
		InvalidCallWeightWitness,
		/// The call was not whitelisted.
		CallIsNotWhitelisted,
		/// The call was already whitelisted; No-Op.
		CallAlreadyWhitelisted,
		/// No deferred dispatch entry exists for this call hash.
		DeferredDispatchNotFound,
		/// The deferred dispatch entry has not yet expired.
		DeferredDispatchNotExpired,
		/// The dispatch has already been deferred.
		AlreadyDeferred,
		/// The deferred dispatch has expired.
		DeferredDispatchExpired,
	}

	#[pallet::storage]
	pub type WhitelistedCall<T: Config> = StorageMap<_, Twox64Concat, T::Hash, (), OptionQuery>;

	/// Deferred dispatches, mapping a call hash to the provided block number at which the deferral
	/// expires and the entry can be permissionlessly removed.
	#[pallet::storage]
	pub type DeferredDispatch<T: Config> =
		StorageMap<_, Twox64Concat, T::Hash, ProvidedBlockNumberFor<T>, OptionQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::whitelist_call())]
		pub fn whitelist_call(origin: OriginFor<T>, call_hash: T::Hash) -> DispatchResult {
			T::WhitelistOrigin::ensure_origin(origin)?;

			ensure!(
				!WhitelistedCall::<T>::contains_key(call_hash),
				Error::<T>::CallAlreadyWhitelisted,
			);

			WhitelistedCall::<T>::insert(call_hash, ());
			T::Preimages::request(&call_hash);

			Self::deposit_event(Event::<T>::CallWhitelisted { call_hash });
			Ok(())
		}

		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::remove_whitelisted_call())]
		pub fn remove_whitelisted_call(origin: OriginFor<T>, call_hash: T::Hash) -> DispatchResult {
			T::WhitelistOrigin::ensure_origin(origin)?;

			WhitelistedCall::<T>::take(call_hash).ok_or(Error::<T>::CallIsNotWhitelisted)?;

			T::Preimages::unrequest(&call_hash);

			Self::deposit_event(Event::<T>::WhitelistedCallRemoved { call_hash });

			Ok(())
		}

		#[pallet::call_index(2)]
		#[pallet::weight(
			T::WeightInfo::dispatch_whitelisted_call(*call_encoded_len)
				.saturating_add(*call_weight_witness)
		)]
		pub fn dispatch_whitelisted_call(
			origin: OriginFor<T>,
			call_hash: T::Hash,
			call_encoded_len: u32,
			call_weight_witness: Weight,
		) -> DispatchResultWithPostInfo {
			let relayer = match T::DispatchWhitelistedOrigin::try_origin(origin) {
				Ok(_) if WhitelistedCall::<T>::contains_key(call_hash) => None,
				Ok(_) => {
					Self::defer_dispatch(call_hash)?;
					return Ok(Some(T::WeightInfo::defer_dispatch(0)).into());
				},
				Err(dispatch_origin) => {
					Some(Self::ensure_signed_deferred_dispatch(dispatch_origin, call_hash)?)
				},
			};

			let call_data = T::Preimages::fetch(&call_hash, Some(call_encoded_len))
				.map_err(|_| Error::<T>::UnavailablePreImage)?;

			let call = <T as Config>::RuntimeCall::decode_all_with_depth_limit(
				frame::deps::frame_support::MAX_EXTRINSIC_DEPTH,
				&mut &call_data[..],
			)
			.map_err(|_| Error::<T>::UndecodableCall)?;

			ensure!(
				call.get_dispatch_info().call_weight.all_lte(call_weight_witness),
				Error::<T>::InvalidCallWeightWitness
			);

			// Relayer isn't charged; the privileged direct path still pays.
			let pays_fee = if relayer.is_some() { Pays::No } else { Pays::Yes };

			let call_actual_weight = Self::clean_and_dispatch(call_hash, call);
			if let Some(who) = relayer {
				Self::deposit_event(Event::<T>::DeferredDispatchExecuted { call_hash, who });
			}

			let actual_weight = call_actual_weight.map(|w| {
				w.saturating_add(T::WeightInfo::dispatch_whitelisted_call(call_encoded_len))
			});
			Ok(PostDispatchInfo { actual_weight, pays_fee })
		}

		#[pallet::call_index(3)]
		#[pallet::weight({
			let call_weight = call.get_dispatch_info().call_weight;
			let call_len = call.encoded_size() as u32;
			T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_len)
				.saturating_add(call_weight)
		})]
		pub fn dispatch_whitelisted_call_with_preimage(
			origin: OriginFor<T>,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResultWithPostInfo {
			let call_hash = T::Hashing::hash_of(&call).into();
			let call_len = call.encoded_size() as u32;

			let relayer = match T::DispatchWhitelistedOrigin::try_origin(origin) {
				Ok(_) if WhitelistedCall::<T>::contains_key(call_hash) => None,
				Ok(_) => {
					Self::defer_dispatch(call_hash)?;
					return Ok(Some(T::WeightInfo::defer_dispatch(call_len)).into());
				},
				Err(dispatch_origin) => {
					Some(Self::ensure_signed_deferred_dispatch(dispatch_origin, call_hash)?)
				},
			};

			// Relayer isn't charged; the privileged direct path still pays.
			let pays_fee = if relayer.is_some() { Pays::No } else { Pays::Yes };

			let call_actual_weight = Self::clean_and_dispatch(call_hash, *call);
			if let Some(who) = relayer {
				Self::deposit_event(Event::<T>::DeferredDispatchExecuted { call_hash, who });
			}

			let actual_weight = call_actual_weight.map(|w| {
				w.saturating_add(T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_len))
			});
			Ok(PostDispatchInfo { actual_weight, pays_fee })
		}

		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::remove_deferred_dispatch())]
		pub fn remove_deferred_dispatch(
			origin: OriginFor<T>,
			call_hash: T::Hash,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			let expire_at = DeferredDispatch::<T>::get(call_hash)
				.ok_or(Error::<T>::DeferredDispatchNotFound)?;

			let now = T::BlockNumberProvider::current_block_number();

			ensure!(now >= expire_at, Error::<T>::DeferredDispatchNotExpired);

			DeferredDispatch::<T>::remove(call_hash);

			Self::deposit_event(Event::<T>::DeferredDispatchRemoved { call_hash });

			Ok(Pays::No.into())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Defer the dispatch of a whitelisted call to a future block.
	///
	/// This function stores the call hash for later execution by any signed origin
	/// before the expiration block.
	fn defer_dispatch(call_hash: T::Hash) -> DispatchResult {
		let now = T::BlockNumberProvider::current_block_number();

		let expire_at = now.saturating_add(T::DeferredDispatchExpiration::get());

		ensure!(!DeferredDispatch::<T>::contains_key(call_hash), Error::<T>::AlreadyDeferred);

		DeferredDispatch::<T>::insert(call_hash, expire_at);

		Self::deposit_event(Event::<T>::DispatchDeferred { call_hash });

		Ok(())
	}

	/// Deferred dispatch sanity check.
	///
	/// Validates that:
	/// - The origin is a signed account.
	/// - A deferred dispatch entry exists for the call hash.
	/// - The deferred dispatch has not yet expired.
	/// - The call is still whitelisted.
	///
	/// The whitelist is always re-checked so that revoking the whitelist (via
	/// [`Pallet::remove_whitelisted_call`]) prevents a relayer from executing a still-deferred
	/// call.
	///
	/// Returns the signed account ID if all checks pass.
	fn ensure_signed_deferred_dispatch(
		origin: T::RuntimeOrigin,
		call_hash: T::Hash,
	) -> Result<T::AccountId, DispatchError> {
		let who = ensure_signed(origin)?;

		let expire_at =
			DeferredDispatch::<T>::get(call_hash).ok_or(Error::<T>::DeferredDispatchNotFound)?;

		ensure!(
			T::BlockNumberProvider::current_block_number() < expire_at,
			Error::<T>::DeferredDispatchExpired
		);

		ensure!(WhitelistedCall::<T>::contains_key(call_hash), Error::<T>::CallIsNotWhitelisted);

		Ok(who)
	}

	/// Clean whitelisting/preimage and dispatch call.
	///
	/// Returns the inner call's actual weight.
	fn clean_and_dispatch(call_hash: T::Hash, call: <T as Config>::RuntimeCall) -> Option<Weight> {
		WhitelistedCall::<T>::remove(call_hash);
		T::Preimages::unrequest(&call_hash);
		DeferredDispatch::<T>::remove(call_hash);

		let result = call.dispatch(frame_system::Origin::<T>::Root.into());

		let call_actual_weight = match result {
			Ok(call_post_info) => call_post_info.actual_weight,
			Err(call_err) => call_err.post_info.actual_weight,
		};
		Self::deposit_event(Event::<T>::WhitelistedCallDispatched { call_hash, result });

		call_actual_weight
	}
}
