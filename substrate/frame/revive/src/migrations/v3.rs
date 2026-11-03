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

//! # Multi-Block Migration v3
//!
//! Iterates `frame_system::Account` and for each account:
//! - If unmapped: calls `map_no_deposit_unchecked` to create a deposit-free mapping.
//! - If already mapped: releases the held address mapping deposit.

use super::PALLET_MIGRATIONS_ID;
use crate::{AddressMapper, Config, HoldReason, LOG_TARGET, weights::WeightInfo};
use frame_support::{
	migrations::{MigrationId, SteppedMigration, SteppedMigrationError},
	pallet_prelude::PhantomData,
	traits::{fungible::MutateHold, tokens::Precision},
	weights::WeightMeter,
};

#[cfg(feature = "try-runtime")]
extern crate alloc;

#[cfg(feature = "try-runtime")]
use alloc::vec::Vec;

/// Maps all existing accounts that are not yet address-mapped.
pub struct Migration<T: Config>(PhantomData<T>);

impl<T: Config> SteppedMigration for Migration<T> {
	type Cursor = T::AccountId;
	type Identifier = MigrationId<17>;

	fn id() -> Self::Identifier {
		MigrationId { pallet_id: *PALLET_MIGRATIONS_ID, version_from: 2, version_to: 3 }
	}

	fn step(
		mut cursor: Option<Self::Cursor>,
		meter: &mut WeightMeter,
	) -> Result<Option<Self::Cursor>, SteppedMigrationError> {
		let required = <T as Config>::WeightInfo::v3_migration_step();
		if meter.remaining().any_lt(required) {
			return Err(SteppedMigrationError::InsufficientWeight { required });
		}

		loop {
			if meter.try_consume(required).is_err() {
				break;
			}

			let mut iter = if let Some(ref last_key) = cursor {
				frame_system::Account::<T>::iter_from(frame_system::Account::<T>::hashed_key_for(
					last_key,
				))
			} else {
				frame_system::Account::<T>::iter()
			};

			if let Some((account_id, _)) = iter.next() {
				if T::AddressMapper::is_eth_derived(&account_id) {
					// Eth-derived accounts are stateless mapped, nothing to do.
				} else {
					let _ = T::AddressMapper::map_no_deposit_unchecked(&account_id).inspect_err(
						|err| {
							log::debug!(
								target: LOG_TARGET,
								"Failed to map account {account_id:?}: {err:?}",
							);
						},
					);

					let _ = T::Currency::release_all(
						&HoldReason::AddressMapping.into(),
						&account_id,
						Precision::BestEffort,
					)
					.inspect_err(|err| {
						log::debug!(
							target: LOG_TARGET,
							"Failed to release mapping deposit for {account_id:?}: {err:?}",
						);
					});
				}
				cursor = Some(account_id);
			} else {
				cursor = None;
				break;
			}
		}
		Ok(cursor)
	}

	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
		use sp_core::Get;
		assert!(T::AutoMap::get(), "v3 migration requires AutoMap to be enabled");

		use codec::Encode;
		let unmapped: u32 = frame_system::Account::<T>::iter_keys()
			.filter(|id| !T::AddressMapper::is_mapped(id))
			.count() as u32;
		log::info!(target: LOG_TARGET, "v3: {unmapped} accounts to map");
		Ok(unmapped.encode())
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade(prev: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
		use codec::Decode;
		use frame_support::traits::fungible::InspectHold;
		use sp_runtime::traits::Zero;

		let prev_unmapped =
			u32::decode(&mut &prev[..]).expect("Failed to decode pre_upgrade state");
		let still_unmapped: u32 = frame_system::Account::<T>::iter_keys()
			.filter(|id| !T::AddressMapper::is_mapped(id))
			.count() as u32;
		assert_eq!(
			still_unmapped, 0,
			"v3: {still_unmapped} accounts still unmapped (was {prev_unmapped})",
		);

		// Verify no accounts have address mapping deposits held
		for (account_id, _) in frame_system::Account::<T>::iter() {
			assert!(
				T::Currency::balance_on_hold(
					&crate::HoldReason::AddressMapping.into(),
					&account_id,
				)
				.is_zero(),
				"v3: account {account_id:?} still has address mapping deposit held",
			);
		}

		Ok(())
	}
}

#[test]
fn migrate_to_v3() {
	use crate::{
		Config, OriginalAccount,
		tests::{ExtBuilder, Test},
	};
	use frame_support::{traits::fungible::Mutate, weights::WeightMeter};
	use sp_core::H160;
	use sp_runtime::AccountId32;

	ExtBuilder::default().genesis_config(None).build().execute_with(|| {
		use crate::address::AccountId32Mapper;
		use frame_support::traits::fungible::InspectHold;

		let unmapped_accounts: Vec<AccountId32> =
			(10..15u8).map(|i| AccountId32::new([i; 32])).collect();
		let mapped_accounts: Vec<AccountId32> =
			(15..20u8).map(|i| AccountId32::new([i; 32])).collect();
		let eth_account = {
			let mut bytes = [0xEE; 32];
			bytes[..20].copy_from_slice(&[0xAA; 20]);
			AccountId32::new(bytes)
		};

		// Fund all accounts
		for acc in unmapped_accounts.iter().chain(&mapped_accounts) {
			<Test as Config>::Currency::set_balance(acc, 1_000_000);
		}
		<Test as Config>::Currency::set_balance(&eth_account, 1_000_000);

		// Map some accounts with a deposit (simulating pre-migration state)
		for acc in &mapped_accounts {
			AccountId32Mapper::<Test>::map(acc).unwrap();
			assert!(
				<Test as Config>::Currency::balance_on_hold(
					&crate::HoldReason::AddressMapping.into(),
					acc
				) > 0,
			);
		}

		// Run migration to completion
		let mut cursor = None;
		let mut weight_meter = WeightMeter::new();
		while let Some(new_cursor) = Migration::<Test>::step(cursor, &mut weight_meter).unwrap() {
			cursor = Some(new_cursor);
		}

		// Verify all non-eth accounts are mapped
		for acc in unmapped_accounts.iter().chain(&mapped_accounts) {
			assert!(AccountId32Mapper::<Test>::is_mapped(acc));
			let addr = AccountId32Mapper::<Test>::to_address(acc);
			assert_eq!(OriginalAccount::<Test>::get(addr).as_ref(), Some(acc));
		}

		// Verify deposits were released for previously-mapped accounts
		for acc in &mapped_accounts {
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&crate::HoldReason::AddressMapping.into(),
					acc
				),
				0,
			);
		}

		// Eth-derived accounts should not have entries in OriginalAccount
		let eth_addr = H160::from_slice(&[0xAA; 20]);
		assert!(OriginalAccount::<Test>::get(eth_addr).is_none());
	});
}

#[test]
fn migrate_to_v3_maps_all_accounts() {
	use crate::{
		Config,
		address::AccountId32Mapper,
		tests::{ExtBuilder, Test},
	};
	use frame_support::{traits::fungible::Mutate, weights::WeightMeter};
	use sp_runtime::AccountId32;

	ExtBuilder::default().genesis_config(None).build().execute_with(|| {
		let accounts: Vec<AccountId32> = (10..15u8).map(|i| AccountId32::new([i; 32])).collect();
		for acc in &accounts {
			<Test as Config>::Currency::set_balance(acc, 1_000_000);
			AccountId32Mapper::<Test>::map(acc).unwrap();
		}

		// Clear all mappings to simulate pre-migration state
		for acc in &accounts {
			AccountId32Mapper::<Test>::unmap(acc).unwrap();
			assert!(!AccountId32Mapper::<Test>::is_mapped(acc));
		}

		// Run migration to completion
		let mut cursor = None;
		let mut meter = WeightMeter::new();
		while let Some(new_cursor) = Migration::<Test>::step(cursor, &mut meter).unwrap() {
			cursor = Some(new_cursor);
		}

		for acc in &accounts {
			assert!(
				AccountId32Mapper::<Test>::is_mapped(acc),
				"account should be mapped after migration"
			);
		}
	});
}
