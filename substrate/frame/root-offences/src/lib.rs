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

//! # Root Offences Pallet
//! Pallet that allows the root to create an offence.
//!
//! NOTE: This pallet should be used for testing purposes.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

extern crate alloc;
use alloc::{vec, vec::Vec};
pub use pallet::*;
use pallet_session::historical::IdentificationTuple;
use sp_runtime::{traits::Convert, Perbill};
use sp_staking::offence::{Kind, Offence, OnOffenceHandler};

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;
	use sp_staking::{offence::ReportOffence, SessionIndex};

	/// Custom offence type for testing spam scenarios.
	///
	/// This allows creating offences with arbitrary kinds and time slots.
	#[derive(Clone, Debug, Encode, Decode, TypeInfo)]
	pub struct TestSpamOffence<Offender> {
		/// The validator being slashed
		pub offender: Offender,
		/// The session in which the offence occurred
		pub session_index: SessionIndex,
		/// Custom time slot (allows unique offences within same session)
		pub time_slot: u128,
		/// Slash fraction to apply
		pub slash_fraction: Perbill,
	}

	impl<Offender: Clone> Offence<Offender> for TestSpamOffence<Offender> {
		const ID: Kind = *b"spamspamspamspam";
		type Slot = u128;

		fn offenders(&self) -> Vec<Offender> {
			vec![self.offender.clone()]
		}

		fn session_index(&self) -> SessionIndex {
			self.session_index
		}

		fn slot(&self) -> Self::Slot {
			self.time_slot
		}

		fn slash_fraction(&self, _offenders_count: u32) -> Perbill {
			self.slash_fraction
		}

		fn validator_set_count(&self) -> u32 {
			unreachable!()
		}
	}

	#[pallet::config]
	pub trait Config:
		frame_system::Config
		+ pallet_session::Config<ValidatorId = <Self as frame_system::Config>::AccountId>
		+ pallet_session::historical::Config
	{
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The offence handler provided by the runtime.
		///
		/// This is a way to give the offence directly to the handling system (staking, ah-client).
		type OffenceHandler: OnOffenceHandler<Self::AccountId, IdentificationTuple<Self>, Weight>;

		/// The offence report system provided by the runtime.
		///
		/// This is a way to give the offence to the `pallet-offences` next.
		type ReportOffence: ReportOffence<
			Self::AccountId,
			IdentificationTuple<Self>,
			TestSpamOffence<IdentificationTuple<Self>>,
		>;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// An offence was created by root.
		OffenceCreated { offenders: Vec<(T::AccountId, Perbill)> },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Failed to get the active era from the staking pallet.
		FailedToGetActiveEra,
	}

	type OffenceDetails<T> = sp_staking::offence::OffenceDetails<
		<T as frame_system::Config>::AccountId,
		IdentificationTuple<T>,
	>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Allows the `root`, for example sudo to create an offence.
		///
		/// If `identifications` is `Some`, then the given identification is used for offence. Else,
		/// it is fetched live from `session::Historical`.
		#[pallet::call_index(0)]
		#[pallet::weight(T::DbWeight::get().reads(2))]
		pub fn create_offence(
			origin: OriginFor<T>,
			offenders: Vec<(T::AccountId, Perbill)>,
			maybe_identifications: Option<Vec<T::FullIdentification>>,
			maybe_session_index: Option<SessionIndex>,
		) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(
				maybe_identifications.as_ref().map_or(true, |ids| ids.len() == offenders.len()),
				"InvalidIdentificationLength"
			);

			let identifications =
				maybe_identifications.ok_or("Unreachable-NoIdentification").or_else(|_| {
					offenders
						.iter()
						.map(|(who, _)| {
							T::FullIdentificationOf::convert(who.clone())
								.ok_or("failed to call FullIdentificationOf")
						})
						.collect::<Result<Vec<_>, _>>()
				})?;

			let slash_fraction =
				offenders.clone().into_iter().map(|(_, fraction)| fraction).collect::<Vec<_>>();
			let offence_details = Self::get_offence_details(offenders.clone(), identifications)?;

			Self::submit_offence(&offence_details, &slash_fraction, maybe_session_index);
			Self::deposit_event(Event::OffenceCreated { offenders });
			Ok(())
		}

		/// Same as [`Pallet::create_offence`], but it reports the offence directly to a
		/// [`Config::ReportOffence`], aka pallet-offences first.
		///
		/// This is useful for more accurate testing of the e2e offence processing pipeline, as it
		/// won't skip the `pallet-offences` step.
		///
		/// It generates an offence of type [`TestSpamOffence`], with cas a fixed `ID`, but can have
		/// any `time_slot`, `session_index``, and `slash_fraction`. These values are the inputs of
		/// transaction, int the same order, with an `IdentiticationTuple` coming first.
		#[pallet::call_index(1)]
		#[pallet::weight(T::DbWeight::get().reads(2))]
		pub fn report_offence(
			origin: OriginFor<T>,
			offences: Vec<(IdentificationTuple<T>, SessionIndex, u128, u32)>,
		) -> DispatchResult {
			ensure_root(origin)?;

			for (offender, session_index, time_slot, slash_ppm) in offences {
				let slash_fraction = Perbill::from_parts(slash_ppm);
				Self::deposit_event(Event::OffenceCreated {
					offenders: vec![(offender.0.clone(), slash_fraction)],
				});
				let offence =
					TestSpamOffence { offender, session_index, time_slot, slash_fraction };

				T::ReportOffence::report_offence(Default::default(), offence).unwrap();
			}

			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Returns a vector of offenders that are going to be slashed.
		fn get_offence_details(
			offenders: Vec<(T::AccountId, Perbill)>,
			identifications: Vec<T::FullIdentification>,
		) -> Result<Vec<OffenceDetails<T>>, DispatchError> {
			Ok(offenders
				.clone()
				.into_iter()
				.zip(identifications.into_iter())
				.map(|((o, _), i)| OffenceDetails::<T> {
					offender: (o.clone(), i),
					reporters: Default::default(),
				})
				.collect())
		}

		/// Submits the offence by calling the `on_offence` function.
		fn submit_offence(
			offenders: &[OffenceDetails<T>],
			slash_fraction: &[Perbill],
			maybe_session_index: Option<SessionIndex>,
		) {
			let session_index = maybe_session_index.unwrap_or_else(|| {
				<pallet_session::Pallet<T> as frame_support::traits::ValidatorSet<
						T::AccountId,
					>>::session_index()
			});
			T::OffenceHandler::on_offence(&offenders, &slash_fraction, session_index);
		}
	}
}
