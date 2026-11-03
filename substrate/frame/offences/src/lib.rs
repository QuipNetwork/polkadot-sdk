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

//! # Offences Pallet
//!
//! Tracks reported offences

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

pub mod migration;
mod mock;
mod tests;

extern crate alloc;

use alloc::vec::Vec;
use codec::Encode;
use core::marker::PhantomData;
use frame_support::weights::Weight;
use sp_runtime::{traits::Hash, Perbill};
use sp_staking::{
	offence::{Kind, Offence, OffenceDetails, OffenceError, OnOffenceHandler, ReportOffence},
	SessionIndex,
};

pub use pallet::*;

/// A binary blob which represents a SCALE codec-encoded `O::Slot`.
type OpaqueSlot = Vec<u8>;

/// A type alias for a report identifier.
type ReportIdOf<T> = <T as frame_system::Config>::Hash;

const LOG_TARGET: &str = "runtime::offences";

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	#[pallet::without_storage_info]
	pub struct Pallet<T>(_);

	/// The pallet's config trait.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
		/// Full identification of the validator.
		type IdentificationTuple: Parameter;
		/// A handler called for every offence report.
		type OnOffenceHandler: OnOffenceHandler<Self::AccountId, Self::IdentificationTuple, Weight>;
	}

	/// The primary structure that holds all offence records keyed by report identifiers.
	#[pallet::storage]
	pub type Reports<T: Config> = StorageMap<
		_,
		Twox64Concat,
		ReportIdOf<T>,
		OffenceDetails<T::AccountId, T::IdentificationTuple>,
	>;

	/// A vector of reports of the same kind that happened at the same time slot.
	#[pallet::storage]
	pub type ConcurrentReportsIndex<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		Kind,
		Twox64Concat,
		OpaqueSlot,
		Vec<ReportIdOf<T>>,
		ValueQuery,
	>;

	/// Events type.
	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event {
		/// There is an offence reported of the given `kind` happened at the `session_index` and
		/// (kind-specific) time slot. This event is not deposited for duplicate slashes.
		/// \[kind, slot\].
		Offence { kind: Kind, slot: OpaqueSlot },
	}
}

impl<T, O> ReportOffence<T::AccountId, T::IdentificationTuple, O> for Pallet<T>
where
	T: Config,
	O: Offence<T::IdentificationTuple>,
{
	fn report_offence(reporters: Vec<T::AccountId>, offence: O) -> Result<(), OffenceError> {
		let offenders = offence.offenders();
		let slot = offence.slot();

		// Go through all offenders in the offence report and find all offenders that were spotted
		// in unique reports.
		let TriageOutcome { concurrent_offenders } =
			match Self::triage_offence_report::<O>(reporters, &slot, offenders) {
				Some(triage) => triage,
				// The report contained only duplicates, so there is no need to slash again.
				None => return Err(OffenceError::DuplicateReport),
			};

		let offenders_count = concurrent_offenders.len() as u32;

		// The amount new offenders are slashed
		let new_fraction = offence.slash_fraction(offenders_count);

		let slash_perbill: Vec<_> = (0..concurrent_offenders.len()).map(|_| new_fraction).collect();

		T::OnOffenceHandler::on_offence(
			&concurrent_offenders,
			&slash_perbill,
			offence.session_index(),
		);

		// Deposit the event.
		Self::deposit_event(Event::Offence { kind: O::ID, slot: slot.encode() });

		Ok(())
	}

	fn is_known_offence(offenders: &[T::IdentificationTuple], slot: &O::Slot) -> bool {
		let any_unknown = offenders.iter().any(|offender| {
			let report_id = Self::report_id::<O>(slot, offender);
			!<Reports<T>>::contains_key(&report_id)
		});

		!any_unknown
	}
}

impl<T: Config> Pallet<T> {
	/// Get the offence details from reports of given ID.
	pub fn reports(
		report_id: ReportIdOf<T>,
	) -> Option<OffenceDetails<T::AccountId, T::IdentificationTuple>> {
		Reports::<T>::get(report_id)
	}

	/// Compute the ID for the given report properties.
	///
	/// The report id depends on the offence kind, time slot and the id of offender.
	fn report_id<O: Offence<T::IdentificationTuple>>(
		slot: &O::Slot,
		offender: &T::IdentificationTuple,
	) -> ReportIdOf<T> {
		(O::ID, slot.encode(), offender).using_encoded(T::Hashing::hash)
	}

	/// Triages the offence report and returns the set of offenders that was involved in unique
	/// reports along with the list of the concurrent offences.
	fn triage_offence_report<O: Offence<T::IdentificationTuple>>(
		reporters: Vec<T::AccountId>,
		slot: &O::Slot,
		offenders: Vec<T::IdentificationTuple>,
	) -> Option<TriageOutcome<T>> {
		let mut storage = ReportIndexStorage::<T, O>::load(slot);

		let mut any_new = false;
		for offender in offenders {
			let report_id = Self::report_id::<O>(slot, &offender);

			if !<Reports<T>>::contains_key(&report_id) {
				any_new = true;
				<Reports<T>>::insert(
					&report_id,
					OffenceDetails { offender, reporters: reporters.clone() },
				);

				storage.insert(report_id);
			}
		}

		if any_new {
			// Load report details for the all reports happened at the same time.
			let concurrent_offenders = storage
				.concurrent_reports
				.iter()
				.filter_map(<Reports<T>>::get)
				.collect::<Vec<_>>();

			storage.save();

			Some(TriageOutcome { concurrent_offenders })
		} else {
			None
		}
	}
}

struct TriageOutcome<T: Config> {
	/// Other reports for the same report kinds.
	concurrent_offenders: Vec<OffenceDetails<T::AccountId, T::IdentificationTuple>>,
}

/// An auxiliary struct for working with storage of indexes localized for a specific offence
/// kind (specified by the `O` type parameter).
///
/// This struct is responsible for aggregating storage writes and the underlying storage should not
/// accessed directly meanwhile.
#[must_use = "The changes are not saved without called `save`"]
struct ReportIndexStorage<T: Config, O: Offence<T::IdentificationTuple>> {
	slot: OpaqueSlot,
	concurrent_reports: Vec<ReportIdOf<T>>,
	_phantom: PhantomData<O>,
}

impl<T: Config, O: Offence<T::IdentificationTuple>> ReportIndexStorage<T, O> {
	/// Preload indexes from the storage for the specific `slot` and the kind of the offence.
	fn load(slot: &O::Slot) -> Self {
		let slot = slot.encode();

		let concurrent_reports = <ConcurrentReportsIndex<T>>::get(&O::ID, &slot);

		Self { slot, concurrent_reports, _phantom: Default::default() }
	}

	/// Insert a new report to the index.
	fn insert(&mut self, report_id: ReportIdOf<T>) {
		// Update the list of concurrent reports.
		self.concurrent_reports.push(report_id);
	}

	/// Dump the indexes to the storage.
	fn save(self) {
		<ConcurrentReportsIndex<T>>::insert(&O::ID, &self.slot, &self.concurrent_reports);
	}
}
