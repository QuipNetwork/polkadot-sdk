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

//! Storage migrations for the dmp pallet.
//!
//! [`MigrateV0ToV1`] translates the old `Vec`-per-para layout
//! ([`v0::DownwardMessageQueues`]) into the paged layout
//! ([`DownwardMessageQueueMeta`] + [`DownwardMessageQueuePages`]).

use super::{inbound_downward_queue::InboundDownwardQueue, *};
#[cfg(feature = "try-runtime")]
use alloc::collections::btree_map::BTreeMap;
use alloc::vec::Vec;
use codec::{Decode, Encode, MaxEncodedLen};
use frame_support::{
	migrations::{MigrationId, SteppedMigration, SteppedMigrationError},
	pallet_prelude::ValueQuery,
	storage_alias,
	weights::WeightMeter,
	Twox64Concat,
};
use scale_info::TypeInfo;

/// Resume position for [`MigrateV0ToV1`]. Returning `Ok(None)` ends the migration; while
/// `v0::DownwardMessageQueues` has data, return `Some(_)`.
#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, PartialEq, Eq, Clone, Debug)]
pub enum MigrationCursor {
	/// Resume by taking the next entry from `v0::DownwardMessageQueues::iter()`.
	Iterate,
	/// Resume mid-para: progress is encoded in the v0 entry itself (the unmigrated suffix
	/// has been written back into `v0::DownwardMessageQueues[para]`).
	InProgress { para: ParaId },
}

/// The in-code storage version.
pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

/// Identifier for migrations of this pallet.
const PALLET_MIGRATIONS_ID: &[u8; 21] = b"cumulus-dmp-queue-mbm";

/// The OLD (pre-paged) storage layout.
pub mod v0 {
	use super::*;

	#[storage_alias]
	pub type DownwardMessageQueues<T: Config> = StorageMap<
		crate::dmp::Pallet<T>,
		Twox64Concat,
		ParaId,
		Vec<InboundDownwardMessage<BlockNumberFor<T>>>,
		ValueQuery,
	>;
}

/// Migrate `v0::DownwardMessageQueues` into the paged layout.
pub struct MigrateV0ToV1<T>(core::marker::PhantomData<T>);

impl<T: Config> SteppedMigration for MigrateV0ToV1<T> {
	type Cursor = MigrationCursor;
	type Identifier = MigrationId<21>;

	fn id() -> Self::Identifier {
		MigrationId { pallet_id: *PALLET_MIGRATIONS_ID, version_from: 0, version_to: 1 }
	}

	fn step(
		mut cursor: Option<Self::Cursor>,
		meter: &mut WeightMeter,
	) -> Result<Option<Self::Cursor>, SteppedMigrationError> {
		if Pallet::<T>::on_chain_storage_version() != Self::id().version_from as u16 {
			return Ok(None);
		}
		let base = <T as Config>::WeightInfo::migrate_v0_to_v1_step_base();
		let per_iter = <T as Config>::WeightInfo::migrate_v0_to_v1_step_iter();
		let per_msg = <T as Config>::WeightInfo::migrate_v0_to_v1_step_msg();

		// Headroom for at least one full iteration; otherwise this call makes no progress.
		let minimum = base.saturating_add(per_iter).saturating_add(per_msg);
		if meter.remaining().any_lt(minimum) {
			return Err(SteppedMigrationError::InsufficientWeight { required: minimum });
		}
		meter.consume(base);

		loop {
			if meter.try_consume(per_iter).is_err() {
				break;
			}

			let para = match cursor.take() {
				Some(MigrationCursor::InProgress { para }) => para,
				Some(MigrationCursor::Iterate) | None => {
					let Some(p) = v0::DownwardMessageQueues::<T>::iter_keys().next() else {
						cursor = None;
						break;
					};
					p
				},
			};

			let msgs = v0::DownwardMessageQueues::<T>::take(para);
			if msgs.is_empty() {
				cursor = Some(MigrationCursor::Iterate);
				continue;
			}

			let total = msgs.len();
			let mut migrated = 0usize;

			for msg in &msgs {
				if migrated > 0 && meter.try_consume(per_msg).is_err() {
					break;
				}
				if InboundDownwardQueue::<T>::push_back_inbound_v1(para, msg).is_err() {
					v0::DownwardMessageQueues::<T>::insert(para, msgs[migrated..].to_vec());
					cursor = Some(MigrationCursor::InProgress { para });
					return Ok(cursor);
				}
				migrated = migrated.saturating_add(1);
			}

			if migrated < total {
				v0::DownwardMessageQueues::<T>::insert(para, msgs[migrated..].to_vec());
				cursor = Some(MigrationCursor::InProgress { para });
				break;
			}

			super::Pallet::<T>::deposit_event(Event::DmpQueueV0Cleaned { para });
			cursor = Some(MigrationCursor::Iterate);
		}

		if cursor.is_none() {
			StorageVersion::new(Self::id().version_to as u16).put::<Pallet<T>>();
		}
		Ok(cursor)
	}

	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
		// Idempotent: snapshot whatever v0 holds now.
		let snapshot: BTreeMap<ParaId, Vec<InboundDownwardMessage<BlockNumberFor<T>>>> =
			v0::DownwardMessageQueues::<T>::iter().collect();

		Ok(snapshot.encode())
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade(_prev: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
		assert_eq!(
			v0::DownwardMessageQueues::<T>::iter().count(),
			0,
			"v0::DownwardMessageQueues still has entries after MigrateV0ToV1",
		);

		Ok(())
	}
}
