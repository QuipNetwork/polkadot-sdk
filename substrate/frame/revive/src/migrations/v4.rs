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

//! # Multi-Block Migration v4
//!
//! Switches storage deposits from the native currency to PGAS.
//!
//! Phase 1 iterates [`crate::CodeInfoOf`] and for each uploaded code records the uploader's
//! existing native `CodeUploadDepositReserve` contribution under [`crate::NativeDepositOf`],
//! keyed by the pallet's own account (the holder of that deposit). The native currency itself
//! stays where it is.
//!
//! Phase 2 iterates [`crate::AccountInfoOf`] and for each contract burn the native
//! `StorageDepositReserve` hold and replaces it with the same amount of PGAS minted into the
//! contract and held under the same reason.
//!
//! Phase 3 rewrites the [`crate::DeletionQueue`] entries from their old `TrieId` value into the
//! new [`crate::storage::DeletionQueueItem`] format.
//!
//! Phases 1 and 2 are skipped when [`crate::Config::Deposit`] does not support PGAS (i.e. the
//! default `()` backend); only phase 3 runs in that case.

use super::PALLET_MIGRATIONS_ID;
#[cfg(feature = "try-runtime")]
use crate::BalanceOf;
use crate::{
	AccountInfoOf, CodeInfoOf, Config, DeletionQueue, HoldReason, LOG_TARGET, NativeDepositOf,
	Pallet, TrieId,
	address::AddressMapper,
	deposit_payment::Deposit,
	storage::{AccountType, DeletionQueueItem},
	weights::WeightInfo,
};
use codec::{Decode, Encode, MaxEncodedLen};
use core::marker::PhantomData;
use frame_support::{
	Twox64Concat,
	migrations::{MigrationId, SteppedMigration, SteppedMigrationError},
	storage_alias,
	weights::WeightMeter,
};
use scale_info::TypeInfo;
use sp_core::{H160, H256};
use sp_runtime::traits::{Saturating, TrailingZeroInput};

extern crate alloc;

#[cfg(feature = "try-runtime")]
use alloc::{collections::btree_map::BTreeMap, vec::Vec};

/// Three-phase cursor: code uploads, contracts, then deletion-queue rewrite.
#[derive(Clone, Encode, Decode, MaxEncodedLen, TypeInfo, PartialEq, Eq, Debug)]
pub enum Cursor {
	/// Last code hash processed in phase 1 (`CodeInfoOf` iteration).
	CodeUpload(H256),
	/// Last contract address processed in phase 2 (`AccountInfoOf` iteration).
	///
	/// `None` is the transition sentinel from phase 1 to phase 2.
	Contract(Option<H160>),
	/// Last deletion-queue index processed in phase 3 (`DeletionQueue` rewrite).
	///
	/// `None` is the transition sentinel from phase 2 to phase 3.
	DeletionQueue(Option<u32>),
}

/// Switches native storage deposits over to PGAS.
pub struct Migration<T>(PhantomData<T>);

impl<T: Config> SteppedMigration for Migration<T> {
	type Cursor = Cursor;
	type Identifier = MigrationId<17>;

	fn id() -> Self::Identifier {
		MigrationId { pallet_id: *PALLET_MIGRATIONS_ID, version_from: 3, version_to: 4 }
	}

	fn step(
		mut cursor: Option<Self::Cursor>,
		meter: &mut WeightMeter,
	) -> Result<Option<Self::Cursor>, SteppedMigrationError> {
		let code_step = <T as Config>::WeightInfo::v4_code_upload_step();
		let contract_step = <T as Config>::WeightInfo::v4_contract_step();
		let deletion_queue_step = <T as Config>::WeightInfo::v4_deletion_queue_step();
		let required = code_step.max(contract_step).max(deletion_queue_step);
		if !meter.can_consume(required) {
			return Err(SteppedMigrationError::InsufficientWeight { required });
		}

		loop {
			let step_weight = match &cursor {
				None | Some(Cursor::CodeUpload(_)) => code_step,
				Some(Cursor::Contract(_)) => contract_step,
				Some(Cursor::DeletionQueue(_)) => deletion_queue_step,
			};
			if meter.try_consume(step_weight).is_err() {
				break;
			}
			cursor = Self::step_once(cursor);
			if cursor.is_none() {
				break;
			}
		}
		Ok(cursor)
	}

	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
		use crate::deposit_payment::Deposit;

		let mut per_owner: BTreeMap<T::AccountId, BalanceOf<T>> = BTreeMap::new();
		if T::Deposit::SUPPORTS_PGAS {
			for (_hash, info) in CodeInfoOf::<T>::iter() {
				let entry = per_owner.entry(info.owner().clone()).or_default();
				*entry = entry.saturating_add(info.deposit());
			}
		}

		let mut per_contract: BTreeMap<H160, BalanceOf<T>> = BTreeMap::new();
		for (addr, info) in AccountInfoOf::<T>::iter() {
			if !matches!(info.account_type, AccountType::Contract(_)) {
				continue;
			}
			let contract = T::AddressMapper::to_account_id(&addr);
			let total = T::Deposit::total_on_hold(HoldReason::StorageDepositReserve, &contract);
			per_contract.insert(addr, total);
		}

		let deletion_queue: BTreeMap<u32, TrieId> = old::DeletionQueue::<T>::iter().collect();

		Ok((per_owner, per_contract, deletion_queue).encode())
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade(prev: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
		use crate::deposit_payment::Deposit;

		let (per_owner, per_contract, deletion_queue) = <(
			BTreeMap<T::AccountId, BalanceOf<T>>,
			BTreeMap<H160, BalanceOf<T>>,
			BTreeMap<u32, TrieId>,
		)>::decode(&mut &prev[..])
		.expect("Failed to decode pre_upgrade state");

		// `NativeDepositOf` is introduced in this migration and starts empty.
		let pallet_account = Pallet::<T>::account_id();
		for (owner, expected) in per_owner {
			let got = NativeDepositOf::<T>::get(&pallet_account, &owner);
			assert_eq!(
				got, expected,
				"v4: NativeDepositOf[pallet][{owner:?}] = {got:?}, expected {expected:?}",
			);
		}

		for (addr, expected) in per_contract {
			let contract = T::AddressMapper::to_account_id(&addr);
			let total = T::Deposit::total_on_hold(HoldReason::StorageDepositReserve, &contract);
			assert_eq!(
				total, expected,
				"v4: contract {addr:?} total_on_hold changed: {total:?} != pre-migration {expected:?}",
			);
		}

		let zero_account = T::AccountId::decode(&mut TrailingZeroInput::zeroes())
			.expect("zero input decodes to a valid AccountId; qed");
		for (key, trie_id) in deletion_queue {
			let got = DeletionQueue::<T>::get(key);
			let expected = DeletionQueueItem::<T>::new(trie_id, zero_account.clone());
			assert_eq!(
				got,
				Some(expected),
				"v4: DeletionQueue[{key}] not rewritten into the new format",
			);
		}
		Ok(())
	}
}

/// Pre-v4 storage layouts.
pub(crate) mod old {
	use super::*;

	/// Pre-v4 layout: a single [`TrieId`] per slot, no associated contract account. We only
	/// iterate it; new entries are written via the live [`crate::DeletionQueue`].
	#[storage_alias]
	pub(crate) type DeletionQueue<T: Config> = StorageMap<Pallet<T>, Twox64Concat, u32, TrieId>;
}

impl<T: Config> Migration<T> {
	/// Run a single iteration of the migration's inner loop, returning the next cursor or
	/// `None` if the migration is complete.
	pub(crate) fn step_once(cursor: Option<Cursor>) -> Option<Cursor> {
		// Without PGAS support phases 1 and 2 are no-ops, so skip straight to phase 3.
		// Forced on under `runtime-benchmarks` so per-phase weights are still measured.
		if !T::Deposit::SUPPORTS_PGAS && !cfg!(feature = "runtime-benchmarks") {
			return match cursor {
				None | Some(Cursor::CodeUpload(_)) | Some(Cursor::Contract(_)) => {
					Some(Cursor::DeletionQueue(None))
				},
				Some(Cursor::DeletionQueue(last)) => match Self::step_3_deletion_queue(last) {
					Some(next) => Some(Cursor::DeletionQueue(Some(next))),
					None => None,
				},
			};
		}

		match cursor {
			None | Some(Cursor::CodeUpload(_)) => {
				let last = if let Some(Cursor::CodeUpload(h)) = cursor { Some(h) } else { None };
				Self::step_1_code_upload(last)
			},
			Some(Cursor::Contract(last)) => Some(match Self::step_2_contract(last) {
				Some(next) => Cursor::Contract(Some(next)),
				None => Cursor::DeletionQueue(None),
			}),
			Some(Cursor::DeletionQueue(last)) => match Self::step_3_deletion_queue(last) {
				Some(next) => Some(Cursor::DeletionQueue(Some(next))),
				None => None,
			},
		}
	}

	/// Phase 1: credit the next `CodeInfoOf` entry's owner in [`NativeDepositOf`]. Returns
	/// `Some(Cursor::Contract(None))` when phase 1 is exhausted.
	fn step_1_code_upload(last: Option<H256>) -> Option<Cursor> {
		let mut iter = match last {
			Some(last) => CodeInfoOf::<T>::iter_from(CodeInfoOf::<T>::hashed_key_for(last)),
			None => CodeInfoOf::<T>::iter(),
		};

		let Some((hash, info)) = iter.next() else { return Some(Cursor::Contract(None)) };

		let pallet_account = Pallet::<T>::account_id();
		NativeDepositOf::<T>::mutate(&pallet_account, info.owner(), |entitlement| {
			*entitlement = entitlement.saturating_add(info.deposit());
		});
		Some(Cursor::CodeUpload(hash))
	}

	/// Phase 2: hand the next contract to [`Deposit::migrate_native_to_pgas`]. EOAs are
	/// skipped but still advance the cursor.
	fn step_2_contract(last: Option<H160>) -> Option<H160> {
		use frame_support::traits::fungible::InspectHold;

		let mut iter = match last {
			Some(last) => AccountInfoOf::<T>::iter_from(AccountInfoOf::<T>::hashed_key_for(last)),
			None => AccountInfoOf::<T>::iter(),
		};

		let (addr, info) = iter.next()?;
		if matches!(info.account_type, AccountType::Contract(_)) {
			let contract = T::AddressMapper::to_account_id(&addr);
			let held =
				T::Currency::balance_on_hold(&HoldReason::StorageDepositReserve.into(), &contract);
			if let Err(err) = T::Deposit::migrate_native_to_pgas(
				HoldReason::StorageDepositReserve,
				&contract,
				held,
			) {
				log::error!(
					target: LOG_TARGET,
					"v4: failed to migrate native -> PGAS deposit for contract {addr:?}: {err:?}",
				);
			}
		}
		Some(addr)
	}

	/// Phase 3: rewrite the next [`DeletionQueue`] slot from the old `TrieId`-only layout
	/// into the new [`DeletionQueueItem`] format. Pre-v4 entries had no [`NativeDepositOf`]
	/// rows, so the recorded `account_id` is a zero placeholder; phase 1 of the deletion
	/// processor will clear an empty prefix on it. Returns `None` when phase 3 finishes.
	fn step_3_deletion_queue(last: Option<u32>) -> Option<u32> {
		let mut iter = match last {
			Some(last) => {
				old::DeletionQueue::<T>::iter_from(old::DeletionQueue::<T>::hashed_key_for(last))
			},
			None => old::DeletionQueue::<T>::iter(),
		};

		let (key, trie_id) = iter.next()?;
		// Same physical slot as `old::DeletionQueue`; the insert overwrites the legacy value
		// with the new encoding.
		let zero_account = T::AccountId::decode(&mut TrailingZeroInput::zeroes())
			.expect("zero input decodes to a valid AccountId; qed");
		DeletionQueue::<T>::insert(key, DeletionQueueItem::<T>::new(trie_id, zero_account));
		Some(key)
	}
}

#[cfg(any(feature = "runtime-benchmarks", feature = "try-runtime", test))]
impl<T: Config> Migration<T> {
	/// Drive the migration to completion. Test/benchmark helper.
	pub fn run_to_completion() {
		let mut cursor: Option<Cursor> = None;
		let mut meter = WeightMeter::new();
		while let Ok(Some(next)) = <Self as SteppedMigration>::step(cursor, &mut meter) {
			cursor = Some(next);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		CodeInfo, FreezeReason,
		storage::{AccountInfo, ContractInfo},
		tests::{Assets, AssetsFreezer, AssetsHolder, ExtBuilder, PGasAssetId, Test},
	};
	use frame_support::traits::fungible::{
		Inspect as _, InspectHold as _, Mutate as _, MutateHold as _,
	};
	use sp_runtime::AccountId32;

	type V4 = Migration<Test>;

	fn seed_code_upload(hash: H256, owner: AccountId32, deposit: u128) {
		let pallet_account = Pallet::<Test>::account_id();
		let ed = <Test as Config>::Currency::minimum_balance();
		<Test as Config>::Currency::mint_into(&pallet_account, ed).unwrap();
		<Test as Config>::Currency::mint_into(&pallet_account, deposit).unwrap();
		<Test as Config>::Currency::hold(
			&HoldReason::CodeUploadDepositReserve.into(),
			&pallet_account,
			deposit,
		)
		.unwrap();
		CodeInfoOf::<Test>::insert(hash, CodeInfo::<Test>::new_with_deposit(owner, deposit));
	}

	fn seed_contract(address: H160, code_hash: H256, storage_deposit: u128) {
		let contract_account = <Test as Config>::AddressMapper::to_account_id(&address);
		let info = ContractInfo::<Test>::new(&address, 0u32.into(), code_hash).unwrap();
		AccountInfoOf::<Test>::insert(
			address,
			AccountInfo::<Test> { account_type: AccountType::Contract(info), dust: 0 },
		);

		let ed = <Test as Config>::Currency::minimum_balance();
		<Test as Config>::Currency::mint_into(&contract_account, ed).unwrap();
		<Test as Config>::Currency::mint_into(&contract_account, storage_deposit).unwrap();
		<Test as Config>::Currency::hold(
			&HoldReason::StorageDepositReserve.into(),
			&contract_account,
			storage_deposit,
		)
		.unwrap();
	}

	#[test]
	fn phase_one_populates_native_deposit_for_code_upload() {
		ExtBuilder::default().genesis_config(None).build().execute_with(|| {
			let pallet_account = Pallet::<Test>::account_id();
			let owner_a = AccountId32::new([1; 32]);
			let owner_b = AccountId32::new([2; 32]);
			seed_code_upload(H256::repeat_byte(0xAA), owner_a.clone(), 1_000);
			seed_code_upload(H256::repeat_byte(0xAB), owner_a.clone(), 500);
			seed_code_upload(H256::repeat_byte(0xBB), owner_b.clone(), 2_000);

			V4::run_to_completion();

			assert_eq!(
				NativeDepositOf::<Test>::get(&pallet_account, &owner_a),
				1_500,
				"owner_a sum of code deposits"
			);
			assert_eq!(
				NativeDepositOf::<Test>::get(&pallet_account, &owner_b),
				2_000,
				"owner_b sum of code deposits"
			);

			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::CodeUploadDepositReserve.into(),
					&pallet_account,
				),
				3_500,
			);
		});
	}

	#[test]
	fn phase_two_burns_native_and_mints_pgas_on_contracts() {
		ExtBuilder::default().genesis_config(None).build().execute_with(|| {
			let owner = AccountId32::new([1; 32]);
			let hash = H256::repeat_byte(0xCC);
			seed_code_upload(hash, owner.clone(), 0);

			let c1 = H160::repeat_byte(0x10);
			let c2 = H160::repeat_byte(0x20);
			seed_contract(c1, hash, 700);
			seed_contract(c2, hash, 1_300);

			let c1_acc = <Test as Config>::AddressMapper::to_account_id(&c1);
			let c2_acc = <Test as Config>::AddressMapper::to_account_id(&c2);

			let total_issuance_before = <Test as Config>::Currency::total_issuance();

			V4::run_to_completion();

			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::StorageDepositReserve.into(),
					&c1_acc,
				),
				0,
			);
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::StorageDepositReserve.into(),
					&c2_acc,
				),
				0,
			);

			assert_eq!(
				total_issuance_before - <Test as Config>::Currency::total_issuance(),
				700 + 1_300,
			);

			use frame_support::traits::tokens::fungibles::{Inspect, InspectHold};
			let pgas_ed = Assets::minimum_balance(PGasAssetId::get());
			assert_eq!(
				AssetsHolder::balance_on_hold(
					PGasAssetId::get(),
					&HoldReason::StorageDepositReserve.into(),
					&c1_acc,
				),
				700,
			);
			assert_eq!(
				AssetsHolder::balance_on_hold(
					PGasAssetId::get(),
					&HoldReason::StorageDepositReserve.into(),
					&c2_acc,
				),
				1_300,
			);
			// Each migrated contract also gets the PGAS ED minted into its free balance and
			// frozen under `FreezeReason::PGasMinBalance`, matching the post-`init_contract`
			// invariant.
			assert_eq!(Assets::balance(PGasAssetId::get(), &c1_acc), pgas_ed);
			assert_eq!(Assets::balance(PGasAssetId::get(), &c2_acc), pgas_ed);
			use frame_support::traits::tokens::fungibles::InspectFreeze;
			assert_eq!(
				AssetsFreezer::balance_frozen(
					PGasAssetId::get(),
					&FreezeReason::PGasMinBalance.into(),
					&c1_acc,
				),
				pgas_ed,
			);
			assert_eq!(
				AssetsFreezer::balance_frozen(
					PGasAssetId::get(),
					&FreezeReason::PGasMinBalance.into(),
					&c2_acc,
				),
				pgas_ed,
			);
		});
	}

	#[test]
	fn phase_three_rewrites_legacy_deletion_queue_entries() {
		use crate::{
			DeletionQueueCounter,
			storage::{DeletionQueueItem, DeletionQueueManager},
		};

		ExtBuilder::default().genesis_config(None).build().execute_with(|| {
			let trie_a: TrieId = vec![0xAA; 16].try_into().unwrap();
			let trie_b: TrieId = vec![0xBB; 24].try_into().unwrap();
			old::DeletionQueue::<Test>::insert(0u32, trie_a.clone());
			old::DeletionQueue::<Test>::insert(1u32, trie_b.clone());
			let mut q = DeletionQueueManager::<Test>::from_test_values(2, 0);
			DeletionQueueCounter::<Test>::set(q.clone());
			let _ = &mut q;

			V4::run_to_completion();

			let zero = AccountId32::new([0u8; 32]);
			assert_eq!(
				DeletionQueue::<Test>::get(0u32),
				Some(DeletionQueueItem::<Test>::new(trie_a, zero.clone())),
			);
			assert_eq!(
				DeletionQueue::<Test>::get(1u32),
				Some(DeletionQueueItem::<Test>::new(trie_b, zero)),
			);
		});
	}

	#[test]
	fn eoa_accounts_are_skipped() {
		use crate::test_utils::{ALICE, ALICE_ADDR, BOB, BOB_ADDR};
		use frame_support::traits::tokens::fungibles::InspectHold;

		ExtBuilder::default().genesis_config(None).build().execute_with(|| {
			let _ = <Test as Config>::Currency::mint_into(&ALICE, Pallet::<Test>::min_balance());
			let _ = <Test as Config>::Currency::mint_into(&BOB, Pallet::<Test>::min_balance());
			AccountInfoOf::<Test>::insert(
				ALICE_ADDR,
				AccountInfo::<Test> { account_type: AccountType::EOA, dust: 0 },
			);

			let owner = AccountId32::new([1; 32]);
			let hash = H256::repeat_byte(0xDD);
			seed_code_upload(hash, owner.clone(), 0);
			seed_contract(BOB_ADDR, hash, 400);

			V4::run_to_completion();

			assert_eq!(
				AssetsHolder::balance_on_hold(
					PGasAssetId::get(),
					&HoldReason::StorageDepositReserve.into(),
					&BOB,
				),
				400,
			);
		});
	}
}
