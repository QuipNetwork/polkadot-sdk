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

//! A module that is responsible for migration of storage.

pub mod v5;
pub mod v6;
pub mod v7;

use crate::{Config, OverweightIndex, Pallet, QueueConfig, QueueConfigData, DEFAULT_POV_SIZE};
use alloc::vec::Vec;
use cumulus_primitives_core::XcmpMessageFormat;
use frame_support::{
	pallet_prelude::*,
	traits::{EnqueueMessage, StorageVersion, UncheckedOnRuntimeUpgrade},
	weights::{constants::WEIGHT_REF_TIME_PER_MILLIS, Weight},
};

/// The in-code storage version.
pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(7);

pub const LOG: &str = "runtime::xcmp-queue-migration";

mod v1 {
	use super::*;
	use codec::{Decode, Encode};

	#[frame_support::storage_alias]
	pub(crate) type QueueConfig<T: Config> = StorageValue<Pallet<T>, QueueConfigData, ValueQuery>;

	#[derive(Encode, Decode, Debug)]
	pub struct QueueConfigData {
		pub suspend_threshold: u32,
		pub drop_threshold: u32,
		pub resume_threshold: u32,
		pub threshold_weight: u64,
		pub weight_restrict_decay: u64,
		pub xcmp_max_individual_weight: u64,
	}

	impl Default for QueueConfigData {
		fn default() -> Self {
			QueueConfigData {
				suspend_threshold: 2,
				drop_threshold: 5,
				resume_threshold: 1,
				threshold_weight: 100_000,
				weight_restrict_decay: 2,
				xcmp_max_individual_weight: 20u64 * WEIGHT_REF_TIME_PER_MILLIS,
			}
		}
	}
}

pub mod v2 {
	use super::*;

	#[frame_support::storage_alias]
	pub(crate) type QueueConfig<T: Config> = StorageValue<Pallet<T>, QueueConfigData, ValueQuery>;

	#[derive(Copy, Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
	pub struct QueueConfigData {
		pub suspend_threshold: u32,
		pub drop_threshold: u32,
		pub resume_threshold: u32,
		pub threshold_weight: Weight,
		pub weight_restrict_decay: Weight,
		pub xcmp_max_individual_weight: Weight,
	}

	impl Default for QueueConfigData {
		fn default() -> Self {
			Self {
				suspend_threshold: 2,
				drop_threshold: 5,
				resume_threshold: 1,
				threshold_weight: Weight::from_parts(100_000, 0),
				weight_restrict_decay: Weight::from_parts(2, 0),
				xcmp_max_individual_weight: Weight::from_parts(
					20u64 * WEIGHT_REF_TIME_PER_MILLIS,
					DEFAULT_POV_SIZE,
				),
			}
		}
	}

	/// Migrates `QueueConfigData` from v1 (using only reference time weights) to v2 (with
	/// 2D weights).
	pub struct UncheckedMigrationToV2<T: Config>(PhantomData<T>);

	impl<T: Config> UncheckedOnRuntimeUpgrade for UncheckedMigrationToV2<T> {
		#[allow(deprecated)]
		fn on_runtime_upgrade() -> Weight {
			let translate = |pre: v1::QueueConfigData| -> v2::QueueConfigData {
				v2::QueueConfigData {
					suspend_threshold: pre.suspend_threshold,
					drop_threshold: pre.drop_threshold,
					resume_threshold: pre.resume_threshold,
					threshold_weight: Weight::from_parts(pre.threshold_weight, 0),
					weight_restrict_decay: Weight::from_parts(pre.weight_restrict_decay, 0),
					xcmp_max_individual_weight: Weight::from_parts(
						pre.xcmp_max_individual_weight,
						DEFAULT_POV_SIZE,
					),
				}
			};

			if v2::QueueConfig::<T>::translate(|pre| pre.map(translate)).is_err() {
				tracing::error!(
					target: crate::LOG_TARGET,
					"unexpected error when performing translation of the QueueConfig type \
					during storage upgrade to v2"
				);
			}

			T::DbWeight::get().reads_writes(1, 1)
		}
	}

	/// [`UncheckedMigrationToV2`] wrapped in a
	/// [`VersionedMigration`](frame_support::migrations::VersionedMigration), ensuring the
	/// migration is only performed when on-chain version is 1.
	#[allow(dead_code)]
	pub type MigrationToV2<T> = frame_support::migrations::VersionedMigration<
		1,
		2,
		UncheckedMigrationToV2<T>,
		Pallet<T>,
		<T as frame_system::Config>::DbWeight,
	>;
}

pub mod v3 {
	use super::*;
	use crate::*;

	/// Status of the inbound XCMP channels.
	#[frame_support::storage_alias]
	pub(crate) type InboundXcmpStatus<T: Config> =
		StorageValue<Pallet<T>, Vec<InboundChannelDetails>, OptionQuery>;

	/// Inbound aggregate XCMP messages. It can only be one per ParaId/block.
	#[frame_support::storage_alias]
	pub(crate) type InboundXcmpMessages<T: Config> = StorageDoubleMap<
		Pallet<T>,
		Blake2_128Concat,
		ParaId,
		Twox64Concat,
		RelayBlockNumber,
		Vec<u8>,
		OptionQuery,
	>;

	#[frame_support::storage_alias]
	pub(crate) type QueueConfig<T: Config> =
		StorageValue<Pallet<T>, v2::QueueConfigData, ValueQuery>;

	#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, TypeInfo)]
	pub struct InboundChannelDetails {
		/// The `ParaId` of the parachain that this channel is connected with.
		pub sender: ParaId,
		/// The state of the channel.
		pub state: InboundState,
		/// The ordered metadata of each inbound message.
		///
		/// Contains info about the relay block number that the message was sent at, and the format
		/// of the incoming message.
		pub message_metadata: Vec<(RelayBlockNumber, XcmpMessageFormat)>,
	}

	#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, Debug, TypeInfo)]
	pub enum InboundState {
		Ok,
		Suspended,
	}

	/// Migrates the pallet storage to v3.
	pub struct UncheckedMigrationToV3<T: Config>(PhantomData<T>);

	impl<T: Config> UncheckedOnRuntimeUpgrade for UncheckedMigrationToV3<T> {
		fn on_runtime_upgrade() -> Weight {
			#[frame_support::storage_alias]
			type Overweight<T: Config> =
				CountedStorageMap<Pallet<T>, Twox64Concat, OverweightIndex, ParaId>;
			let overweight_messages = Overweight::<T>::initialize_counter() as u64;

			T::DbWeight::get().reads_writes(overweight_messages, 1)
		}
	}

	/// [`UncheckedMigrationToV3`] wrapped in a
	/// [`VersionedMigration`](frame_support::migrations::VersionedMigration), ensuring the
	/// migration is only performed when on-chain version is 2.
	pub type MigrationToV3<T> = frame_support::migrations::VersionedMigration<
		2,
		3,
		UncheckedMigrationToV3<T>,
		Pallet<T>,
		<T as frame_system::Config>::DbWeight,
	>;

	pub fn lazy_migrate_inbound_queue<T: Config>() {
		let Some(mut states) = v3::InboundXcmpStatus::<T>::get() else {
			tracing::debug!(target: LOG, "Lazy migration finished: item gone");
			return;
		};
		let Some(ref mut next) = states.first_mut() else {
			tracing::debug!(target: LOG, "Lazy migration finished: item empty");
			v3::InboundXcmpStatus::<T>::kill();
			return;
		};
		tracing::debug!(
			target: LOG,
			sibling=?next.sender,
			msgs_left=%next.message_metadata.len(),
			"Migrating inbound HRMP channel."
		);
		// We take the last element since the MQ is a FIFO and we want to keep the order.
		let Some((block_number, format)) = next.message_metadata.pop() else {
			states.remove(0);
			v3::InboundXcmpStatus::<T>::put(states);
			return;
		};
		if format != XcmpMessageFormat::ConcatenatedVersionedXcm {
			tracing::warn!(
				target: LOG,
				?format,
				"Dropping message (not ConcatenatedVersionedXcm)"
			);
			v3::InboundXcmpMessages::<T>::remove(&next.sender, &block_number);
			v3::InboundXcmpStatus::<T>::put(states);
			return;
		}

		let Some(msg) = v3::InboundXcmpMessages::<T>::take(&next.sender, &block_number) else {
			defensive!("Storage corrupted: HRMP message missing:", (next.sender, block_number));
			v3::InboundXcmpStatus::<T>::put(states);
			return;
		};

		let Ok(msg): Result<BoundedVec<_, _>, _> = msg.try_into() else {
			tracing::error!(target: LOG, "Message dropped: too big");
			v3::InboundXcmpStatus::<T>::put(states);
			return;
		};

		// Finally! We have a proper message.
		T::XcmpQueue::enqueue_message(msg.as_bounded_slice(), next.sender);
		tracing::debug!(target: LOG, next_sender=?next.sender, ?block_number, "Migrated HRMP message to MQ");
		v3::InboundXcmpStatus::<T>::put(states);
	}
}

pub mod v4 {
	use super::*;

	/// Migrates `QueueConfigData` to v4, removing deprecated fields and bumping page
	/// thresholds to at least the default values.
	pub struct UncheckedMigrationToV4<T: Config>(PhantomData<T>);

	impl<T: Config> UncheckedOnRuntimeUpgrade for UncheckedMigrationToV4<T> {
		fn on_runtime_upgrade() -> Weight {
			let translate = |pre: v2::QueueConfigData| -> QueueConfigData {
				let pre_default = v2::QueueConfigData::default();
				// If the previous values are the default ones, let's replace them with the new
				// default.
				if pre.suspend_threshold == pre_default.suspend_threshold &&
					pre.drop_threshold == pre_default.drop_threshold &&
					pre.resume_threshold == pre_default.resume_threshold
				{
					return QueueConfigData::default();
				}

				// If the previous values are not the default ones, let's leave them as they are.
				QueueConfigData {
					suspend_threshold: pre.suspend_threshold,
					drop_threshold: pre.drop_threshold,
					resume_threshold: pre.resume_threshold,
				}
			};

			if QueueConfig::<T>::translate(|pre| pre.map(translate)).is_err() {
				tracing::error!(
					target: crate::LOG_TARGET,
					"unexpected error when performing translation of the QueueConfig type \
					during storage upgrade to v4"
				);
			}

			T::DbWeight::get().reads_writes(1, 1)
		}
	}

	/// [`UncheckedMigrationToV4`] wrapped in a
	/// [`VersionedMigration`](frame_support::migrations::VersionedMigration), ensuring the
	/// migration is only performed when on-chain version is 3.
	pub type MigrationToV4<T> = frame_support::migrations::VersionedMigration<
		3,
		4,
		UncheckedMigrationToV4<T>,
		Pallet<T>,
		<T as frame_system::Config>::DbWeight,
	>;
}

#[cfg(all(feature = "try-runtime", test))]
mod tests {
	use super::*;
	use crate::mock::{new_test_ext, Test};
	use frame_support::traits::OnRuntimeUpgrade;

	#[test]
	#[allow(deprecated)]
	fn test_migration_to_v2() {
		let v1 = v1::QueueConfigData {
			suspend_threshold: 5,
			drop_threshold: 12,
			resume_threshold: 3,
			threshold_weight: 333_333,
			weight_restrict_decay: 1,
			xcmp_max_individual_weight: 10_000_000_000,
		};

		new_test_ext().execute_with(|| {
			let storage_version = StorageVersion::new(1);
			storage_version.put::<Pallet<Test>>();

			frame_support::storage::unhashed::put_raw(
				&crate::QueueConfig::<Test>::hashed_key(),
				&v1.encode(),
			);

			let bytes = v2::MigrationToV2::<Test>::pre_upgrade();
			assert!(bytes.is_ok());
			v2::MigrationToV2::<Test>::on_runtime_upgrade();
			assert!(v2::MigrationToV2::<Test>::post_upgrade(bytes.unwrap()).is_ok());

			let v2 = v2::QueueConfig::<Test>::get();

			assert_eq!(v1.suspend_threshold, v2.suspend_threshold);
			assert_eq!(v1.drop_threshold, v2.drop_threshold);
			assert_eq!(v1.resume_threshold, v2.resume_threshold);
			assert_eq!(v1.threshold_weight, v2.threshold_weight.ref_time());
			assert_eq!(v1.weight_restrict_decay, v2.weight_restrict_decay.ref_time());
			assert_eq!(v1.xcmp_max_individual_weight, v2.xcmp_max_individual_weight.ref_time());
		});
	}

	#[test]
	#[allow(deprecated)]
	fn test_migration_to_v4() {
		new_test_ext().execute_with(|| {
			let storage_version = StorageVersion::new(3);
			storage_version.put::<Pallet<Test>>();

			let v2 = v2::QueueConfigData {
				drop_threshold: 5,
				suspend_threshold: 2,
				resume_threshold: 1,
				..Default::default()
			};

			frame_support::storage::unhashed::put_raw(
				&crate::QueueConfig::<Test>::hashed_key(),
				&v2.encode(),
			);

			let bytes = v4::MigrationToV4::<Test>::pre_upgrade();
			assert!(bytes.is_ok());
			v4::MigrationToV4::<Test>::on_runtime_upgrade();
			assert!(v4::MigrationToV4::<Test>::post_upgrade(bytes.unwrap()).is_ok());

			let v4 = QueueConfig::<Test>::get();

			assert_eq!(
				v4,
				QueueConfigData { suspend_threshold: 32, drop_threshold: 48, resume_threshold: 8 }
			);
		});

		new_test_ext().execute_with(|| {
			let storage_version = StorageVersion::new(3);
			storage_version.put::<Pallet<Test>>();

			let v2 = v2::QueueConfigData {
				drop_threshold: 100,
				suspend_threshold: 50,
				resume_threshold: 40,
				..Default::default()
			};

			frame_support::storage::unhashed::put_raw(
				&crate::QueueConfig::<Test>::hashed_key(),
				&v2.encode(),
			);

			let bytes = v4::MigrationToV4::<Test>::pre_upgrade();
			assert!(bytes.is_ok());
			v4::MigrationToV4::<Test>::on_runtime_upgrade();
			assert!(v4::MigrationToV4::<Test>::post_upgrade(bytes.unwrap()).is_ok());

			let v4 = QueueConfig::<Test>::get();

			assert_eq!(
				v4,
				QueueConfigData {
					suspend_threshold: 50,
					drop_threshold: 100,
					resume_threshold: 40
				}
			);
		});
	}
}
