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

use super::*;
use crate::types::RegionRecord;
use codec::{Decode, Encode};
use core::marker::PhantomData;
use frame_support::traits::{Get, UncheckedOnRuntimeUpgrade};
use sp_runtime::Saturating;

#[cfg(feature = "try-runtime")]
use alloc::vec::Vec;
#[cfg(feature = "try-runtime")]
use frame_support::ensure;

mod v1 {
	use super::*;

	/// V0 region record.
	#[derive(Encode, Decode)]
	struct RegionRecordV0<AccountId, Balance> {
		/// The end of the Region.
		pub end: Timeslice,
		/// The owner of the Region.
		pub owner: AccountId,
		/// The amount paid to Polkadot for this Region, or `None` if renewal is not allowed.
		pub paid: Option<Balance>,
	}

	pub struct MigrateToV1Impl<T>(PhantomData<T>);

	impl<T: Config> UncheckedOnRuntimeUpgrade for MigrateToV1Impl<T> {
		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let mut count: u64 = 0;

			<Regions<T>>::translate::<RegionRecordV0<T::AccountId, BalanceOf<T>>, _>(|_, v0| {
				count.saturating_inc();
				Some(RegionRecord { end: v0.end, owner: Some(v0.owner), paid: v0.paid })
			});

			log::info!(
				target: LOG_TARGET,
				"Storage migration v1 for pallet-broker finished.",
			);

			// calculate and return migration weights
			T::DbWeight::get().reads_writes(count as u64 + 1, count as u64 + 1)
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			Ok((Regions::<T>::iter_keys().count() as u32).encode())
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let old_count = u32::decode(&mut &state[..]).expect("Known good");
			let new_count = Regions::<T>::iter_values().count() as u32;

			ensure!(old_count == new_count, "Regions count should not change");
			Ok(())
		}
	}
}

mod v2 {
	use super::*;
	use frame_support::{
		pallet_prelude::{OptionQuery, Twox64Concat},
		storage_alias,
	};

	#[storage_alias]
	pub type AllowedRenewals<T: Config> = StorageMap<
		Pallet<T>,
		Twox64Concat,
		PotentialRenewalId,
		PotentialRenewalRecordOf<T>,
		OptionQuery,
	>;

	pub struct MigrateToV2Impl<T>(PhantomData<T>);

	impl<T: Config> UncheckedOnRuntimeUpgrade for MigrateToV2Impl<T> {
		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let mut count = 0;
			for (renewal_id, renewal) in AllowedRenewals::<T>::drain() {
				PotentialRenewals::<T>::insert(renewal_id, renewal);
				count += 1;
			}

			log::info!(
				target: LOG_TARGET,
				"Storage migration v2 for pallet-broker finished.",
			);

			// calculate and return migration weights
			T::DbWeight::get().reads_writes(count as u64 + 1, count as u64 + 1)
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			Ok((AllowedRenewals::<T>::iter_keys().count() as u32).encode())
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let old_count = u32::decode(&mut &state[..]).expect("Known good");
			let new_count = PotentialRenewals::<T>::iter_values().count() as u32;

			ensure!(old_count == new_count, "Renewal count should not change");
			Ok(())
		}
	}
}

mod v3 {
	use super::*;
	use codec::MaxEncodedLen;
	use frame_support::{
		pallet_prelude::{Debug, OptionQuery, TypeInfo},
		storage_alias,
	};
	use frame_system::Pallet as System;
	use sp_arithmetic::Perbill;

	pub struct MigrateToV3Impl<T>(PhantomData<T>);

	impl<T: Config> UncheckedOnRuntimeUpgrade for MigrateToV3Impl<T> {
		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let acc = Pallet::<T>::account_id();
			System::<T>::inc_providers(&acc);
			// calculate and return migration weights
			T::DbWeight::get().writes(1)
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			Ok(System::<T>::providers(&Pallet::<T>::account_id()).encode())
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let old_providers = u32::decode(&mut &state[..]).expect("Known good");
			let new_providers = System::<T>::providers(&Pallet::<T>::account_id()) as u32;

			ensure!(new_providers == old_providers + 1, "Providers count should increase by one");
			Ok(())
		}
	}

	#[storage_alias]
	pub type Configuration<T: Config> = StorageValue<Pallet<T>, ConfigRecordOf<T>, OptionQuery>;
	pub type ConfigRecordOf<T> =
		ConfigRecord<frame_system::pallet_prelude::BlockNumberFor<T>, RelayBlockNumberOf<T>>;

	// types added here for v4 migration
	#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
	pub struct ConfigRecord<BlockNumber, RelayBlockNumber> {
		/// The number of Relay-chain blocks in advance which scheduling should be fixed and the
		/// `Coretime::assign` API used to inform the Relay-chain.
		pub advance_notice: RelayBlockNumber,
		/// The length in blocks of the Interlude Period for forthcoming sales.
		pub interlude_length: BlockNumber,
		/// The length in blocks of the Leadin Period for forthcoming sales.
		pub leadin_length: BlockNumber,
		/// The length in timeslices of Regions which are up for sale in forthcoming sales.
		pub region_length: Timeslice,
		/// The proportion of cores available for sale which should be sold in order for the price
		/// to remain the same in the next sale.
		pub ideal_bulk_proportion: Perbill,
		/// An artificial limit to the number of cores which are allowed to be sold. If `Some` then
		/// no more cores will be sold than this.
		pub limit_cores_offered: Option<CoreIndex>,
		/// The amount by which the renewal price increases each sale period.
		pub renewal_bump: Perbill,
		/// The duration by which rewards for contributions to the InstaPool must be collected.
		pub contribution_timeout: Timeslice,
	}

	#[storage_alias]
	pub type SaleInfo<T: Config> = StorageValue<Pallet<T>, SaleInfoRecordOf<T>, OptionQuery>;
	pub type SaleInfoRecordOf<T> =
		SaleInfoRecord<BalanceOf<T>, frame_system::pallet_prelude::BlockNumberFor<T>>;

	/// The status of a Bulk Coretime Sale.
	#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
	pub struct SaleInfoRecord<Balance, BlockNumber> {
		/// The relay block number at which the sale will/did start.
		pub sale_start: BlockNumber,
		/// The length in relay chain blocks of the Leadin Period (where the price is decreasing).
		pub leadin_length: BlockNumber,
		/// The price of Bulk Coretime after the Leadin Period.
		pub price: Balance,
		/// The first timeslice of the Regions which are being sold in this sale.
		pub region_begin: Timeslice,
		/// The timeslice on which the Regions which are being sold in the sale terminate. (i.e.
		/// One after the last timeslice which the Regions control.)
		pub region_end: Timeslice,
		/// The number of cores we want to sell, ideally. Selling this amount would result in no
		/// change to the price for the next sale.
		pub ideal_cores_sold: CoreIndex,
		/// Number of cores which are/have been offered for sale.
		pub cores_offered: CoreIndex,
		/// The index of the first core which is for sale. Core of Regions which are sold have
		/// incrementing indices from this.
		pub first_core: CoreIndex,
		/// The latest price at which Bulk Coretime was purchased until surpassing the ideal number
		/// of cores were sold.
		pub sellout_price: Option<Balance>,
		/// Number of cores which have been sold; never more than cores_offered.
		pub cores_sold: CoreIndex,
	}
}

pub mod v4 {
	use super::*;

	type BlockNumberFor<T> = frame_system::pallet_prelude::BlockNumberFor<T>;

	pub trait BlockToRelayHeightConversion<T: Config> {
		/// Converts absolute value of parachain block number to relay chain block number
		fn convert_block_number_to_relay_height(
			block_number: BlockNumberFor<T>,
		) -> RelayBlockNumberOf<T>;

		/// Converts parachain block length into equivalent relay chain block length
		fn convert_block_length_to_relay_length(
			block_number: BlockNumberFor<T>,
		) -> RelayBlockNumberOf<T>;
	}

	pub struct MigrateToV4Impl<T, BlockConversion>(PhantomData<T>, PhantomData<BlockConversion>);
	impl<T: Config, BlockConversion: BlockToRelayHeightConversion<T>> UncheckedOnRuntimeUpgrade
		for MigrateToV4Impl<T, BlockConversion>
	{
		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			let (interlude_length, configuration_leadin_length) =
				if let Some(config_record) = v3::Configuration::<T>::get() {
					(config_record.interlude_length, config_record.leadin_length)
				} else {
					((0 as u32).into(), (0 as u32).into())
				};

			let updated_interlude_length: RelayBlockNumberOf<T> =
				BlockConversion::convert_block_length_to_relay_length(interlude_length);
			let updated_leadin_length: RelayBlockNumberOf<T> =
				BlockConversion::convert_block_length_to_relay_length(configuration_leadin_length);
			log::info!(target: LOG_TARGET, "Configuration Pre-Migration: Interlude Length {:?}->{:?} Leadin Length {:?}->{:?}", interlude_length, updated_interlude_length, configuration_leadin_length, updated_leadin_length);

			let (sale_start, sale_info_leadin_length) =
				if let Some(sale_info_record) = v3::SaleInfo::<T>::get() {
					(sale_info_record.sale_start, sale_info_record.leadin_length)
				} else {
					((0 as u32).into(), (0 as u32).into())
				};

			let updated_sale_start: RelayBlockNumberOf<T> =
				BlockConversion::convert_block_number_to_relay_height(sale_start);
			let updated_sale_info_leadin_length: RelayBlockNumberOf<T> =
				BlockConversion::convert_block_length_to_relay_length(sale_info_leadin_length);
			log::info!(target: LOG_TARGET, "SaleInfo Pre-Migration: Sale Start {:?}->{:?} Interlude Length {:?}->{:?}", sale_start, updated_sale_start, sale_info_leadin_length, updated_sale_info_leadin_length);

			Ok((interlude_length, configuration_leadin_length, sale_start, sale_info_leadin_length)
				.encode())
		}

		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let mut weight = T::DbWeight::get().reads(1);

			if let Some(config_record) = v3::Configuration::<T>::take() {
				log::info!(target: LOG_TARGET, "migrating Configuration record");

				let updated_interlude_length: RelayBlockNumberOf<T> =
					BlockConversion::convert_block_length_to_relay_length(
						config_record.interlude_length,
					);
				let updated_leadin_length: RelayBlockNumberOf<T> =
					BlockConversion::convert_block_length_to_relay_length(
						config_record.leadin_length,
					);

				let updated_config_record = ConfigRecord {
					interlude_length: updated_interlude_length,
					leadin_length: updated_leadin_length,
					advance_notice: config_record.advance_notice,
					region_length: config_record.region_length,
					ideal_bulk_proportion: config_record.ideal_bulk_proportion,
					limit_cores_offered: config_record.limit_cores_offered,
					renewal_bump: config_record.renewal_bump,
					contribution_timeout: config_record.contribution_timeout,
				};
				Configuration::<T>::put(updated_config_record);
			}
			weight.saturating_accrue(T::DbWeight::get().reads_writes(1, 1));

			if let Some(sale_info) = v3::SaleInfo::<T>::take() {
				log::info!(target: LOG_TARGET, "migrating SaleInfo record");

				let updated_sale_start: RelayBlockNumberOf<T> =
					BlockConversion::convert_block_number_to_relay_height(sale_info.sale_start);
				let updated_leadin_length: RelayBlockNumberOf<T> =
					BlockConversion::convert_block_length_to_relay_length(sale_info.leadin_length);

				let updated_sale_info = SaleInfoRecord {
					sale_start: updated_sale_start,
					leadin_length: updated_leadin_length,
					end_price: sale_info.price,
					region_begin: sale_info.region_begin,
					region_end: sale_info.region_end,
					ideal_cores_sold: sale_info.ideal_cores_sold,
					cores_offered: sale_info.cores_offered,
					first_core: sale_info.first_core,
					sellout_price: sale_info.sellout_price,
					cores_sold: sale_info.cores_sold,
					sale_index: 0,
				};
				SaleInfo::<T>::put(updated_sale_info);
			}

			weight.saturating_add(T::DbWeight::get().reads_writes(1, 2))
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let (
				old_interlude_length,
				old_configuration_leadin_length,
				old_sale_start,
				old_sale_info_leadin_length,
			): (BlockNumberFor<T>, BlockNumberFor<T>, BlockNumberFor<T>, BlockNumberFor<T>) =
				Decode::decode(&mut &state[..]).expect("pre_upgrade provides a valid state; qed");

			if let Some(config_record) = Configuration::<T>::get() {
				ensure!(
					Self::verify_updated_block_length(
						old_configuration_leadin_length,
						config_record.leadin_length
					),
					"must migrate configuration leadin_length"
				);

				ensure!(
					Self::verify_updated_block_length(
						old_interlude_length,
						config_record.interlude_length
					),
					"must migrate configuration interlude_length"
				);
			}

			if let Some(sale_info) = SaleInfo::<T>::get() {
				ensure!(
					Self::verify_updated_block_time(old_sale_start, sale_info.sale_start),
					"must migrate sale info sale_start"
				);

				ensure!(
					Self::verify_updated_block_length(
						old_sale_info_leadin_length,
						sale_info.leadin_length
					),
					"must migrate sale info leadin_length"
				);
			}

			Ok(())
		}
	}

	#[cfg(feature = "try-runtime")]
	impl<T: Config, BlockConversion: BlockToRelayHeightConversion<T>>
		MigrateToV4Impl<T, BlockConversion>
	{
		fn verify_updated_block_time(
			old_value: BlockNumberFor<T>,
			new_value: RelayBlockNumberOf<T>,
		) -> bool {
			BlockConversion::convert_block_number_to_relay_height(old_value) == new_value
		}

		fn verify_updated_block_length(
			old_value: BlockNumberFor<T>,
			new_value: RelayBlockNumberOf<T>,
		) -> bool {
			BlockConversion::convert_block_length_to_relay_length(old_value) == new_value
		}
	}
}

pub mod v5 {
	use super::*;
	use frame_support::traits::Get;

	/// Pre-v5 `SaleInfo` storage: the `SaleInfoRecord` layout without `sale_index`.
	pub(crate) mod old {
		use super::*;
		use codec::MaxEncodedLen;
		use frame_support::{
			pallet_prelude::{Debug, OptionQuery, TypeInfo},
			storage_alias,
		};

		#[storage_alias]
		pub type SaleInfo<T: Config> = StorageValue<Pallet<T>, SaleInfoRecordOf<T>, OptionQuery>;
		pub type SaleInfoRecordOf<T> = SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>;

		/// Pre-v5 `SaleInfoRecord`: the current layout without `sale_index`.
		#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
		pub struct SaleInfoRecord<Balance, RelayBlockNumber> {
			pub sale_start: RelayBlockNumber,
			pub leadin_length: RelayBlockNumber,
			pub end_price: Balance,
			pub region_begin: Timeslice,
			pub region_end: Timeslice,
			pub ideal_cores_sold: CoreIndex,
			pub cores_offered: CoreIndex,
			pub first_core: CoreIndex,
			pub sellout_price: Option<Balance>,
			pub cores_sold: CoreIndex,
		}
	}

	/// Supplies the first sale's `region_begin` (timeslice), used to reconstruct `sale_index`.
	/// Historical data, not recoverable from on-chain storage.
	pub trait FirstSaleRegion {
		fn region_begin() -> Timeslice;
	}

	/// Adds the `sale_index` field to `SaleInfoRecord`.
	pub struct MigrateToV5Impl<T, F>(PhantomData<T>, PhantomData<F>);

	impl<T: Config, F: FirstSaleRegion> UncheckedOnRuntimeUpgrade for MigrateToV5Impl<T, F> {
		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			let sale_info_state = old::SaleInfo::<T>::get()
				.map(|sale| (sale.sale_start, sale.region_begin, sale.region_end));
			Ok(sale_info_state.encode())
		}

		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let mut weight = T::DbWeight::get().reads(1);

			if let Some(old_sale) = old::SaleInfo::<T>::get() {
				let first_region_begin = F::region_begin();

				// `region_length` from Configuration; defaults to 0 (sale_index 1) if absent, so
				// the record is rewritten rather than the migration panicking.
				let region_length = if let Some(config) = Configuration::<T>::get() {
					config.region_length
				} else {
					log::error!(
						target: LOG_TARGET,
						"Migrating SaleInfo: Configuration missing while SaleInfo exists; \
						defaulting region_length to 0 and sale_index to 1",
					);
					0
				};
				weight.saturating_accrue(T::DbWeight::get().reads(1));

				// `region_begin` advances by `region_length` timeslices each sale with no gaps, so
				// completed sales = `(region_begin - first_region_begin) / region_length` and the
				// current index is that plus one. The bootstrap sale in `do_start_sales` is index
				// 0, so the first stored sale is index 1. Counted in timeslices, which are
				// exact.
				let completed_regions = if region_length > 0 {
					old_sale.region_begin.saturating_sub(first_region_begin) / region_length
				} else {
					0
				};
				let sale_index: SaleIndex = completed_regions.saturating_add(1);

				log::info!(
					target: LOG_TARGET,
					"Migrating SaleInfo: first_region_begin={:?}, current_region_begin={:?}, \
					region_length={:?}, sale_index={}",
					first_region_begin,
					old_sale.region_begin,
					region_length,
					sale_index,
				);

				let updated_sale = SaleInfoRecord {
					sale_start: old_sale.sale_start,
					leadin_length: old_sale.leadin_length,
					end_price: old_sale.end_price,
					region_begin: old_sale.region_begin,
					region_end: old_sale.region_end,
					ideal_cores_sold: old_sale.ideal_cores_sold,
					cores_offered: old_sale.cores_offered,
					first_core: old_sale.first_core,
					sellout_price: old_sale.sellout_price,
					cores_sold: old_sale.cores_sold,
					sale_index,
				};
				SaleInfo::<T>::put(updated_sale);
				weight.saturating_accrue(T::DbWeight::get().writes(1));

				log::info!(
					target: LOG_TARGET,
					"Storage migration v5 for pallet-broker completed: added sale_index = {}",
					sale_index,
				);
			} else {
				log::info!(
					target: LOG_TARGET,
					"No SaleInfo found, skipping v5 migration",
				);
			}

			weight
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let old_state: Option<(RelayBlockNumberOf<T>, Timeslice, Timeslice)> =
				Decode::decode(&mut &state[..])
					.map_err(|_| "Failed to decode pre_upgrade state")?;

			// on_runtime_upgrade skips a missing SaleInfo, so only check when one existed.
			if let Some((old_sale_start, old_region_begin, old_region_end)) = old_state {
				let new_sale =
					SaleInfo::<T>::get().ok_or("SaleInfo should still exist after migration")?;

				ensure!(
					old_sale_start == new_sale.sale_start &&
						old_region_begin == new_sale.region_begin &&
						old_region_end == new_sale.region_end,
					"Core SaleInfo fields should not have changed during migration"
				);
				ensure!(
					new_sale.sale_index >= 1,
					"An existing sale must have a sale_index of at least 1"
				);
			}

			Ok(())
		}
	}
}

/// Migrate the pallet storage from `0` to `1`.
pub type MigrateV0ToV1<T> = frame_support::migrations::VersionedMigration<
	0,
	1,
	v1::MigrateToV1Impl<T>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

pub type MigrateV1ToV2<T> = frame_support::migrations::VersionedMigration<
	1,
	2,
	v2::MigrateToV2Impl<T>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

pub type MigrateV2ToV3<T> = frame_support::migrations::VersionedMigration<
	2,
	3,
	v3::MigrateToV3Impl<T>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

pub type MigrateV3ToV4<T, BlockConversion> = frame_support::migrations::VersionedMigration<
	3,
	4,
	v4::MigrateToV4Impl<T, BlockConversion>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

pub type MigrateV4ToV5<T, FirstSaleRegion> = frame_support::migrations::VersionedMigration<
	4,
	5,
	v5::MigrateToV5Impl<T, FirstSaleRegion>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;
