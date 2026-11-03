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

//! Migrates the storage to version 7.

use crate::*;
use frame_support::{pallet_prelude::*, traits::UncheckedOnRuntimeUpgrade};

/// [`VersionedMigration`](frame_support::migrations::VersionedMigration), that is performed only
/// when the on-chain version is 6.
pub type MigrateV6ToV7<T> = frame_support::migrations::VersionedMigration<
	6,
	7,
	unversioned::UncheckedMigrateV6ToV7<T>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

// V6 storage aliases
mod v6 {
	use super::*;

	#[derive(Clone, Eq, PartialEq, Encode, Decode, TypeInfo, Debug, MaxEncodedLen)]
	pub struct OutboundChannelDetails {
		pub recipient: ParaId,
		pub state: OutboundState,
		pub signals_exist: bool,
		pub first_index: u16,
		pub last_index: u16,
		pub flags: OutboundChannelFlags,
	}

	#[frame_support::storage_alias]
	pub(super) type OutboundXcmpStatus<T: Config> = StorageValue<
		Pallet<T>,
		BoundedVec<OutboundChannelDetails, <T as Config>::MaxActiveOutboundChannels>,
		ValueQuery,
	>;
}

mod unversioned {
	use super::*;
	pub struct UncheckedMigrateV6ToV7<T: Config>(PhantomData<T>);
}

impl<T: Config> UncheckedOnRuntimeUpgrade for unversioned::UncheckedMigrateV6ToV7<T> {
	fn on_runtime_upgrade() -> frame_support::weights::Weight {
		// We use `Vec` instead of `BoundedVec` for `pre` in order to avoid any decoding error
		// in case `T::MaxActiveOutboundChannels` is decreased in the same runtime upgrade where
		// the migration is executed.
		let translate = |pre: Vec<v6::OutboundChannelDetails>|
		 -> BoundedVec<OutboundChannelDetails, T::MaxActiveOutboundChannels> {
			BoundedVec::defensive_truncate_from(
				pre.iter()
					.map(|pre_channel_details| OutboundChannelDetails {
						recipient: pre_channel_details.recipient,
						state: pre_channel_details.state,
						signals_exist: pre_channel_details.signals_exist,
						first_index: pre_channel_details.first_index,
						last_index: pre_channel_details.last_index,
						flags: pre_channel_details.flags,
						// Corrects itself over time.
						queued_bytes: 0,
					})
					.collect(),
			)
		};

		if OutboundXcmpStatus::<T>::translate(|pre| pre.map(translate)).is_err() {
			defensive!(
				"unexpected error when performing translation of the OutboundXcmpStatus type \
				during storage upgrade to v7"
			);
		}

		let proof_size = 2 * BoundedVec::<
			OutboundChannelDetails,
			<T as Config>::MaxActiveOutboundChannels,
		>::max_encoded_len();
		Weight::from_parts(0, proof_size as u64)
			.saturating_add(T::DbWeight::get().reads_writes(1, 1))
	}
}
