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

//! Multi-block migration from v0 to v1 for the recovery pallet.

extern crate alloc;

use super::{v0, PALLET_MIGRATIONS_ID};
use crate::{pallet, Pallet};
#[cfg(feature = "try-runtime")]
use alloc::vec::Vec;
use codec::{Decode, Encode, MaxEncodedLen};
use frame_support::{
	migrations::{MigrationId, SteppedMigration, SteppedMigrationError},
	pallet_prelude::PhantomData,
	traits::{
		fungible::MutateHold, Consideration, Get, GetStorageVersion, ReservableCurrency,
		StorageVersion,
	},
	weights::WeightMeter,
	BoundedVec,
};

/// Cursor for tracking migration progress across the three old storage items.
#[derive(Encode, Decode, MaxEncodedLen, Clone, PartialEq, Eq, Debug)]
pub enum MigrationCursor<AccountId: MaxEncodedLen> {
	/// Migrating `Recoverable` storage to `FriendGroups`.
	Recoverable(Option<AccountId>),
	/// Migrating `ActiveRecoveries` storage to `Attempt`.
	ActiveRecoveries(Option<(AccountId, AccountId)>),
	/// Migrating `Proxy` storage to `Inheritor`.
	Proxy(Option<AccountId>),
}

/// Multi-block migration from v0 to v1.
///
/// This migration:
/// 1. Converts `Recoverable` entries to `FriendGroups` with inheritor set to the multisig account
///    derived from friends + threshold (so friends can collectively control inherited accounts via
///    the multisig pallet)
/// 2. Converts `ActiveRecoveries` to `Attempt` entries, preserving approval state
/// 3. Converts `Proxy` entries to `Inheritor` (inverts the mapping)
///
/// All old deposits are unreserved and new consideration tickets are created.
/// Entries that fail to migrate (e.g., due to insufficient funds for new tickets)
/// are logged and skipped - the old deposit is still returned to the user.
pub struct MigrateV0ToV1<T: v0::MigrationConfig>(PhantomData<T>);

impl<T: v0::MigrationConfig> SteppedMigration for MigrateV0ToV1<T> {
	type Cursor = MigrationCursor<T::AccountId>;
	type Identifier = MigrationId<18>;

	fn id() -> Self::Identifier {
		MigrationId { pallet_id: *PALLET_MIGRATIONS_ID, version_from: 0, version_to: 1 }
	}

	fn step(
		cursor: Option<Self::Cursor>,
		meter: &mut WeightMeter,
	) -> Result<Option<Self::Cursor>, SteppedMigrationError> {
		if Pallet::<T>::on_chain_storage_version() != Self::id().version_from as u16 {
			return Ok(None);
		}

		let required = T::DbWeight::get().reads_writes(2, 2);

		if meter.remaining().any_lt(required) {
			return Err(SteppedMigrationError::InsufficientWeight { required });
		}

		let mut cursor = cursor.unwrap_or(MigrationCursor::Recoverable(None));

		loop {
			if meter.try_consume(required).is_err() {
				break;
			}

			match cursor {
				MigrationCursor::Recoverable(last_key) => {
					let mut iter = if let Some(ref key) = last_key {
						v0::Recoverable::<T>::iter_from_key(key)
					} else {
						v0::Recoverable::<T>::iter()
					};

					if let Some((account, config)) = iter.next() {
						// Unreserve the old deposit, we ignore the case that this could reap
						let _ = <T as v0::MigrationConfig>::Currency::unreserve(
							&account,
							config.deposit,
						);

						// We calculate a multisig and use it as inheritor since the old logic did
						// not have a dedicated inheritor.
						let mut sorted_friends = config.friends.to_vec();
						sorted_friends.sort();
						let inheritor =
							v0::multi_account_id::<T::AccountId>(&sorted_friends, config.threshold);
						let friend_group = config.into_v1_friend_group(inheritor);

						let friend_groups = BoundedVec::try_from(alloc::vec![friend_group])
							.expect("ensured by integrity_test; qed");
						let footprint = Pallet::<T>::friend_group_footprint(&friend_groups);

						match T::FriendGroupsConsideration::new(&account, footprint) {
							Ok(ticket) => {
								pallet::FriendGroups::<T>::insert(
									&account,
									(friend_groups, ticket),
								);
							},
							Err(_) => {
								frame_support::defensive!(
									"MigrateV0ToV1: Failed to create FriendGroups ticket, skipping"
								);
							},
						}

						v0::Recoverable::<T>::remove(&account);
						cursor = MigrationCursor::Recoverable(Some(account));
					} else {
						cursor = MigrationCursor::ActiveRecoveries(None);
					}
				},
				MigrationCursor::ActiveRecoveries(last_key) => {
					let mut iter = if let Some((ref lost, ref rescuer)) = last_key {
						v0::ActiveRecoveries::<T>::iter_from(
							v0::ActiveRecoveries::<T>::hashed_key_for(lost, rescuer),
						)
					} else {
						v0::ActiveRecoveries::<T>::iter()
					};

					let Some((lost, rescuer, recovery)) = iter.next() else {
						cursor = MigrationCursor::Proxy(None);
						continue;
					};

					cursor =
						MigrationCursor::ActiveRecoveries(Some((lost.clone(), rescuer.clone())));
					v0::ActiveRecoveries::<T>::remove(&lost, &rescuer);

					// Unreserve the old deposit
					let _ =
						<T as v0::MigrationConfig>::Currency::unreserve(&rescuer, recovery.deposit);

					// Try to find the friend group for this recovery that we already migrated
					let Some((friend_groups, _)) = pallet::FriendGroups::<T>::get(&lost) else {
						frame_support::defensive!(
							"MigrateV0ToV1: Failed to find FriendGroups for lost account"
						);
						continue;
					};

					if friend_groups.len() != 1 {
						frame_support::defensive!(
							"MigrateV0ToV1: Expected exactly one friend group for lost account"
						);
						continue;
					}

					let Some(fg) = friend_groups.first() else {
						frame_support::defensive!(
							"MigrateV0ToV1: Failed to find friend group for lost account"
						);
						continue;
					};

					// Convert vouched friends list to approval bitfield
					let mut approvals = crate::ApprovalBitfieldOf::<T>::default();
					for voucher in recovery.friends.iter() {
						if let Some(index) = fg.friends.iter().position(|f| f == voucher) {
							let _ = approvals.set_if_not_set(index);
						} else {
							frame_support::defensive!(
								"MigrateV0ToV1: Voucher not found in friend group"
							);
							continue;
						}
					}

					let attempt = crate::AttemptOf::<T> {
						friend_group_index: 0, // 0 since there is only one friend group
						initiator: rescuer.clone(),
						init_block: recovery.created,
						last_approval_block: recovery.created,
						approvals,
					};

					let security_deposit = T::SecurityDeposit::get();
					let ticket = match crate::AttemptTicketOf::<T>::new(
						&rescuer,
						Pallet::<T>::attempt_footprint(),
					) {
						Ok(ticket) => ticket,
						Err(e) => {
							log::error!(
								"MigrateV0ToV1: Failed to create Attempt ticket for rescuer {:?}: {:?}",
								rescuer,
								e,
							);
							crate::IdentifiedConsideration {
								depositor: rescuer.clone(),
								ticket: None,
								_phantom: Default::default(),
							}
						},
					};

					let held_deposit = if <T as pallet::Config>::Currency::hold(
						&crate::HoldReason::SecurityDeposit.into(),
						&rescuer,
						security_deposit,
					)
					.is_ok()
					{
						security_deposit
					} else {
						log::warn!(
							"MigrateV0ToV1: Failed to hold security deposit for rescuer; \
							 inserting Attempt with zero deposit"
						);
						Default::default()
					};

					pallet::Attempt::<T>::insert(
						&lost,
						0u32, // group index 0
						(attempt, ticket, held_deposit),
					);
				},
				MigrationCursor::Proxy(last_key) => {
					let mut iter = if let Some(ref key) = last_key {
						v0::Proxy::<T>::iter_from_key(key)
					} else {
						v0::Proxy::<T>::iter()
					};

					let Some((rescuer, lost)) = iter.next() else {
						// only exit return
						StorageVersion::new(Self::id().version_to as u16).put::<Pallet<T>>();
						return Ok(None);
					};
					cursor = MigrationCursor::Proxy(Some(rescuer.clone()));
					v0::Proxy::<T>::remove(&rescuer);

					// All ongoing rescuers got a consumer ref... bad old code
					let _ = frame_system::Pallet::<T>::dec_consumers(&rescuer);

					let inheritor = rescuer.clone();
					let inheritance_priority = 0u32;

					// Create inheritor ticket
					let ticket = match Pallet::<T>::inheritor_ticket(&inheritor) {
						Ok(ticket) => ticket,
						Err(e) => {
							log::error!("MigrateV0ToV1: Failed to create Inheritor ticket for rescuer {:?}: {:?}", inheritor, e);
							crate::IdentifiedConsideration {
								depositor: rescuer.clone(),
								ticket: None,
								_phantom: Default::default(),
							}
						},
					};

					pallet::Inheritor::<T>::insert(
						&lost,
						(inheritance_priority, inheritor, ticket),
					);
				},
			}
		}

		Ok(Some(cursor))
	}

	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, frame_support::sp_runtime::TryRuntimeError> {
		use codec::Encode;

		let recoverable_count = v0::Recoverable::<T>::iter().count() as u32;
		let active_recoveries_count = v0::ActiveRecoveries::<T>::iter().count() as u32;
		let proxy_count = v0::Proxy::<T>::iter().count() as u32;

		log::info!(
			target: "runtime::recovery",
			"MigrateV0ToV1: pre_upgrade - Recoverable: {}, ActiveRecoveries: {}, Proxy: {}",
			recoverable_count,
			active_recoveries_count,
			proxy_count,
		);

		Ok((recoverable_count, active_recoveries_count, proxy_count).encode())
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade(state: Vec<u8>) -> Result<(), frame_support::sp_runtime::TryRuntimeError> {
		use codec::Decode;

		let (recoverable_count, active_recoveries_count, proxy_count) =
			<(u32, u32, u32)>::decode(&mut &state[..]).expect("Failed to decode pre_upgrade state");

		// All old storage should be cleared
		assert_eq!(v0::Recoverable::<T>::iter().count(), 0);
		assert_eq!(v0::ActiveRecoveries::<T>::iter().count(), 0);
		assert_eq!(v0::Proxy::<T>::iter().count(), 0);

		// New storage should be populated
		let friend_groups_count = pallet::FriendGroups::<T>::iter().count() as u32;
		let attempt_count = pallet::Attempt::<T>::iter().count() as u32;
		let inheritor_count = pallet::Inheritor::<T>::iter().count() as u32;

		log::info!(
			target: "runtime::recovery",
			"MigrateV0ToV1: post_upgrade - FriendGroups: {}, Attempt: {}, Inheritor: {}",
			friend_groups_count,
			attempt_count,
			inheritor_count,
		);

		// These can fail for Kusama AH because of buggy accounts...
		if friend_groups_count != recoverable_count {
			log::error!(
				"MigrateV0ToV1: FriendGroups count mismatch: {} != {}",
				friend_groups_count,
				recoverable_count
			);
		}
		if attempt_count != active_recoveries_count {
			log::error!(
				"MigrateV0ToV1: Attempt count mismatch: {} != {}",
				attempt_count,
				active_recoveries_count
			);
		}
		if inheritor_count != proxy_count {
			log::error!(
				"MigrateV0ToV1: Inheritor count mismatch: {} != {}",
				inheritor_count,
				proxy_count
			);
		}

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::{v0, MigrateV0ToV1};
	use crate::{
		mock::{new_test_ext, Balances, Test, ALICE, BOB, CHARLIE, DAVE, EVE},
		pallet,
	};
	use frame_support::{
		migrations::SteppedMigration,
		traits::{GetStorageVersion, ReservableCurrency, StorageVersion},
		weights::WeightMeter,
		BoundedVec,
	};

	type T = Test;

	fn friends(accounts: &[u64]) -> v0::FriendsOf<T> {
		let mut f: Vec<u64> = accounts.to_vec();
		f.sort();
		BoundedVec::try_from(f).unwrap()
	}

	fn run_migration() {
		let mut cursor = None;

		#[cfg(feature = "try-runtime")]
		let data = MigrateV0ToV1::<T>::pre_upgrade().unwrap();

		loop {
			let mut meter = WeightMeter::new();
			match MigrateV0ToV1::<T>::step(cursor, &mut meter) {
				Ok(None) => break,
				Ok(Some(c)) => cursor = Some(c),
				Err(e) => panic!("Migration failed: {:?}", e),
			}
		}

		#[cfg(feature = "try-runtime")]
		MigrateV0ToV1::<T>::post_upgrade(data).unwrap();
	}

	#[test]
	fn migration_works() {
		new_test_ext().execute_with(|| {
			let config_deposit = 50u128;
			let recovery_deposit = 100u128;

			// === Setup v0 storage ===

			// 1. Recoverable configs for ALICE and BOB
			v0::Recoverable::<T>::insert(
				ALICE,
				v0::RecoveryConfig {
					delay_period: 10u64,
					deposit: config_deposit,
					friends: friends(&[BOB, CHARLIE]),
					threshold: 2,
				},
			);
			Balances::reserve(&ALICE, config_deposit).unwrap();

			v0::Recoverable::<T>::insert(
				BOB,
				v0::RecoveryConfig {
					delay_period: 5u64,
					deposit: config_deposit,
					friends: friends(&[ALICE, CHARLIE]),
					threshold: 1,
				},
			);
			Balances::reserve(&BOB, config_deposit).unwrap();

			// EVE has a zero delay period - should be clamped to 1
			v0::Recoverable::<T>::insert(
				EVE,
				v0::RecoveryConfig {
					delay_period: 0u64,
					deposit: config_deposit,
					friends: friends(&[ALICE, BOB]),
					threshold: 1,
				},
			);
			Balances::reserve(&EVE, config_deposit).unwrap();

			// 2. Active recovery: CHARLIE trying to recover ALICE
			v0::ActiveRecoveries::<T>::insert(
				ALICE,
				CHARLIE,
				v0::ActiveRecovery {
					created: 1u64,
					deposit: recovery_deposit,
					friends: friends(&[BOB]),
				},
			);
			Balances::reserve(&CHARLIE, recovery_deposit).unwrap();

			// 3. Proxy: DAVE has recovered EVE's account
			v0::Proxy::<T>::insert(DAVE, EVE);
			frame_system::Pallet::<T>::inc_consumers(&DAVE).unwrap();

			// === Verify v0 state before migration ===
			assert_eq!(v0::Recoverable::<T>::iter().count(), 3);
			assert_eq!(v0::ActiveRecoveries::<T>::iter().count(), 1);
			assert_eq!(v0::Proxy::<T>::iter().count(), 1);
			assert_eq!(Balances::reserved_balance(ALICE), config_deposit);
			assert_eq!(Balances::reserved_balance(BOB), config_deposit);
			assert_eq!(Balances::reserved_balance(CHARLIE), recovery_deposit);
			assert_eq!(Balances::reserved_balance(EVE), config_deposit);
			assert_eq!(frame_system::Pallet::<T>::consumers(&DAVE), 1);

			// === Run migration ===
			run_migration();

			// === Verify v0 storage is cleared ===
			assert_eq!(v0::Recoverable::<T>::iter().count(), 0);
			assert_eq!(v0::ActiveRecoveries::<T>::iter().count(), 0);
			assert_eq!(v0::Proxy::<T>::iter().count(), 0);

			// === Verify v1 storage is populated ===

			// FriendGroups should have entries for ALICE, BOB, and EVE
			assert_eq!(pallet::FriendGroups::<T>::iter().count(), 3);

			// Check ALICE's migrated FriendGroups
			let (alice_groups, _ticket) = pallet::FriendGroups::<T>::get(ALICE).unwrap();
			assert_eq!(alice_groups.len(), 1);
			let alice_fg = &alice_groups[0];
			assert_eq!(alice_fg.friends.len(), 2);
			assert!(alice_fg.friends.contains(&BOB));
			assert!(alice_fg.friends.contains(&CHARLIE));
			assert_eq!(alice_fg.friends_needed, 2);
			assert_eq!(alice_fg.inheritance_delay, 10);
			// Inheritor should be the multisig account derived from friends + threshold
			let expected_inheritor = v0::multi_account_id::<u64>(&[BOB, CHARLIE], 2);
			assert_eq!(alice_fg.inheritor, expected_inheritor);
			assert_eq!(alice_fg.inheritance_priority, 0);

			// Check BOB's migrated FriendGroups
			let (bob_groups, _ticket) = pallet::FriendGroups::<T>::get(BOB).unwrap();
			assert_eq!(bob_groups.len(), 1);
			let bob_fg = &bob_groups[0];
			assert_eq!(bob_fg.friends_needed, 1);
			assert_eq!(bob_fg.inheritance_delay, 5);

			// Check EVE's migrated FriendGroups - cancel_delay clamped from 0 to 1
			let (eve_groups, _ticket) = pallet::FriendGroups::<T>::get(EVE).unwrap();
			assert_eq!(eve_groups.len(), 1);
			let eve_fg = &eve_groups[0];
			assert_eq!(eve_fg.inheritance_delay, 0);
			assert_eq!(eve_fg.cancel_delay, 1);

			// Inheritor should have entry for EVE (lost) -> DAVE (inheritor)
			assert_eq!(pallet::Inheritor::<T>::iter().count(), 1);
			let (order, inheritor, _ticket) = pallet::Inheritor::<T>::get(EVE).unwrap();
			assert_eq!(inheritor, DAVE);
			assert_eq!(order, 0);

			// === Verify Attempt migration ===
			// ActiveRecovery for (ALICE, CHARLIE) should be migrated to Attempt
			assert_eq!(pallet::Attempt::<T>::iter().count(), 1);
			let (attempt, _ticket, deposit) = pallet::Attempt::<T>::get(ALICE, 0u32).unwrap();
			assert_eq!(attempt.initiator, CHARLIE);
			assert_eq!(attempt.friend_group_index, 0);
			assert_eq!(deposit, crate::mock::SECURITY_DEPOSIT);
		});
	}

	#[test]
	fn migration_inserts_attempt_when_security_deposit_hold_fails() {
		new_test_ext().execute_with(|| {
			let config_deposit = 50u128;
			// Old recovery deposit is much smaller than new SECURITY_DEPOSIT (100)
			let old_recovery_deposit = 10u128;

			// Use a fresh account (99) as the rescuer with a very tight balance.
			// They need enough for: attempt ticket + security deposit (100).
			// We give them just enough for the ticket but NOT for the security deposit.
			let rescuer: u64 = 99;
			let lost = ALICE;

			// Give rescuer a small balance: old_recovery_deposit (reserved) + a bit of free
			// balance. After unreserve they'll have ~60 free, which covers the attempt ticket
			// but not SECURITY_DEPOSIT (100).
			let rescuer_free = 50u128;
			pallet_balances::Pallet::<Test>::force_set_balance(
				frame_system::RawOrigin::Root.into(),
				rescuer,
				rescuer_free + old_recovery_deposit,
			)
			.unwrap();
			Balances::reserve(&rescuer, old_recovery_deposit).unwrap();

			// Setup v0 Recoverable for the lost account (required for migration to find friend
			// group)
			v0::Recoverable::<T>::insert(
				lost,
				v0::RecoveryConfig {
					delay_period: 10u64,
					deposit: config_deposit,
					friends: friends(&[BOB, CHARLIE]),
					threshold: 2,
				},
			);
			Balances::reserve(&lost, config_deposit).unwrap();

			// Setup v0 ActiveRecovery: rescuer trying to recover lost's account
			v0::ActiveRecoveries::<T>::insert(
				lost,
				rescuer,
				v0::ActiveRecovery {
					created: 1u64,
					deposit: old_recovery_deposit,
					friends: BoundedVec::default(), // no vouchers yet
				},
			);

			assert_eq!(v0::ActiveRecoveries::<T>::iter().count(), 1);

			// Run migration
			run_migration();

			// The old storage should be cleared
			assert_eq!(v0::ActiveRecoveries::<T>::iter().count(), 0);

			assert_eq!(
				pallet::Attempt::<T>::iter().count(),
				1,
				"Attempt entry was not inserted during migration — active recovery lost!"
			);
		});
	}

	#[test]
	fn migrated_recovery_can_be_completed() {
		use crate::mock::{signed, Recovery};
		use frame_support::assert_ok;

		new_test_ext().execute_with(|| {
			let config_deposit = 50u128;
			let recovery_deposit = 100u128;

			// === Setup v0 storage ===
			// ALICE has a recovery config with BOB, CHARLIE, DAVE as friends, threshold 2
			v0::Recoverable::<T>::insert(
				ALICE,
				v0::RecoveryConfig {
					delay_period: 10u64,
					deposit: config_deposit,
					friends: friends(&[BOB, CHARLIE, DAVE]),
					threshold: 2,
				},
			);
			Balances::reserve(&ALICE, config_deposit).unwrap();

			// BOB started a recovery attempt for ALICE, CHARLIE has already vouched
			v0::ActiveRecoveries::<T>::insert(
				ALICE,
				BOB,
				v0::ActiveRecovery {
					created: 1u64,
					deposit: recovery_deposit,
					friends: friends(&[CHARLIE]), // CHARLIE already vouched
				},
			);
			Balances::reserve(&BOB, recovery_deposit).unwrap();

			// === Run migration ===
			run_migration();

			// === Verify migration worked ===
			assert_eq!(pallet::FriendGroups::<T>::iter().count(), 1);
			assert_eq!(pallet::Attempt::<T>::iter().count(), 1);

			// Compute the expected multisig inheritor (doesn't need to exist as an account)
			let multisig_inheritor = v0::multi_account_id::<u64>(&[BOB, CHARLIE, DAVE], 2);
			// Verify the inheritor is correctly set to the multisig account
			let (groups, _) = pallet::FriendGroups::<T>::get(ALICE).unwrap();
			assert_eq!(groups[0].inheritor, multisig_inheritor);

			// Check the attempt state
			let (attempt, _, _) = pallet::Attempt::<T>::get(ALICE, 0u32).unwrap();
			assert_eq!(attempt.initiator, BOB);
			// CHARLIE's approval should be preserved (index 1 in sorted [BOB, CHARLIE, DAVE])
			assert_eq!(attempt.approvals.count_ones(), 1);

			// === Now complete the recovery using the new pallet ===

			// DAVE approves (this should be the 2nd approval, meeting threshold)
			assert_ok!(Recovery::approve_attempt(signed(DAVE), ALICE, 0));

			// Check we now have 2 approvals
			let (attempt, _, _) = pallet::Attempt::<T>::get(ALICE, 0u32).unwrap();
			assert_eq!(attempt.approvals.count_ones(), 2);

			// Advance blocks past the inheritance_delay (10 blocks)
			frame_system::Pallet::<T>::set_block_number(15);

			// Anyone can finish the attempt now that requirements are met
			assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 0));

			// Verify the multisig is now the inheritor of ALICE's account
			let (order, inheritor, _) = pallet::Inheritor::<T>::get(ALICE).unwrap();
			assert_eq!(inheritor, multisig_inheritor);
			assert_eq!(order, 0);

			// Attempt should be removed after finish
			assert!(pallet::Attempt::<T>::get(ALICE, 0u32).is_none());
		});
	}

	#[test]
	fn migration_bumps_on_chain_storage_version() {
		new_test_ext().execute_with(|| {
			StorageVersion::new(0).put::<pallet::Pallet<T>>();
			assert_eq!(pallet::Pallet::<T>::on_chain_storage_version(), 0);

			v0::Recoverable::<T>::insert(
				ALICE,
				v0::RecoveryConfig {
					delay_period: 10u64,
					deposit: 50u128,
					friends: friends(&[BOB, CHARLIE]),
					threshold: 2,
				},
			);
			Balances::reserve(&ALICE, 50u128).unwrap();

			run_migration();

			assert_eq!(pallet::Pallet::<T>::on_chain_storage_version(), 1);
		});
	}

	#[test]
	fn migration_is_idempotent_after_completion() {
		new_test_ext().execute_with(|| {
			StorageVersion::new(0).put::<pallet::Pallet<T>>();

			v0::Recoverable::<T>::insert(
				ALICE,
				v0::RecoveryConfig {
					delay_period: 10u64,
					deposit: 50u128,
					friends: friends(&[BOB, CHARLIE]),
					threshold: 2,
				},
			);
			Balances::reserve(&ALICE, 50u128).unwrap();

			run_migration();
			assert_eq!(pallet::Pallet::<T>::on_chain_storage_version(), 1);

			let _guard = frame_support::StorageNoopGuard::new();
			let mut meter = WeightMeter::new();
			assert!(matches!(MigrateV0ToV1::<T>::step(None, &mut meter), Ok(None)));
		});
	}

	#[test]
	fn migration_inserts_attempt_when_storage_ticket_fails() {
		new_test_ext().execute_with(|| {
			let config_deposit = 50u128;
			let old_recovery_deposit = 10u128;
			let rescuer: u64 = 99;
			let lost = ALICE;

			pallet_balances::Pallet::<Test>::force_set_balance(
				frame_system::RawOrigin::Root.into(),
				rescuer,
				crate::mock::ExistentialDeposit::get() as u128 + old_recovery_deposit,
			)
			.unwrap();
			Balances::reserve(&rescuer, old_recovery_deposit).unwrap();

			v0::Recoverable::<T>::insert(
				lost,
				v0::RecoveryConfig {
					delay_period: 10u64,
					deposit: config_deposit,
					friends: friends(&[BOB, CHARLIE]),
					threshold: 2,
				},
			);
			Balances::reserve(&lost, config_deposit).unwrap();

			v0::ActiveRecoveries::<T>::insert(
				lost,
				rescuer,
				v0::ActiveRecovery {
					created: 1u64,
					deposit: old_recovery_deposit,
					friends: BoundedVec::default(),
				},
			);

			run_migration();

			assert_eq!(v0::ActiveRecoveries::<T>::iter().count(), 0);
			assert_eq!(
				pallet::Attempt::<T>::iter().count(),
				1,
				"Attempt entry must survive even when storage ticket creation fails",
			);

			let (attempt, ticket, held_deposit) = pallet::Attempt::<T>::get(lost, 0u32).unwrap();
			assert_eq!(attempt.initiator, rescuer);
			assert!(ticket.ticket.is_none(), "Inner ticket must be None when storage hold failed");
			assert_eq!(ticket.depositor, rescuer);
			assert_eq!(held_deposit, 0, "Security deposit must be zero when hold failed");

			use frame::traits::fungible::InspectHold;
			assert_eq!(
				Balances::balance_on_hold(&crate::HoldReason::AttemptStorage.into(), &rescuer),
				0,
			);
			assert_eq!(
				Balances::balance_on_hold(&crate::HoldReason::SecurityDeposit.into(), &rescuer),
				0,
			);
		});
	}

	#[test]
	fn migration_inserts_inheritor_when_ticket_fails() {
		use frame::traits::fungible::InspectHold;

		new_test_ext().execute_with(|| {
			let rescuer: u64 = 99;
			let lost = ALICE;

			pallet_balances::Pallet::<Test>::force_set_balance(
				frame_system::RawOrigin::Root.into(),
				rescuer,
				crate::mock::ExistentialDeposit::get() as u128,
			)
			.unwrap();
			frame_system::Pallet::<T>::inc_consumers(&rescuer).unwrap();

			v0::Proxy::<T>::insert(rescuer, lost);
			assert_eq!(frame_system::Pallet::<T>::consumers(&rescuer), 1);

			run_migration();

			assert_eq!(v0::Proxy::<T>::iter().count(), 0);
			assert_eq!(frame_system::Pallet::<T>::consumers(&rescuer), 0);

			assert_eq!(
				pallet::Inheritor::<T>::iter().count(),
				1,
				"Inheritor entry must survive even when ticket creation fails",
			);
			let (priority, inheritor, ticket) = pallet::Inheritor::<T>::get(lost).unwrap();
			assert_eq!(inheritor, rescuer);
			assert_eq!(priority, 0);
			assert!(ticket.ticket.is_none(), "Inner ticket must be None when hold failed");
			assert_eq!(ticket.depositor, rescuer);

			assert_eq!(
				Balances::balance_on_hold(&crate::HoldReason::InheritorStorage.into(), &rescuer),
				0,
			);
		});
	}
}
