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

//! V0 storage definitions for the recovery pallet.
//!
//! These represent the old storage layout before the v1 migration.

use codec::{Decode, Encode, MaxEncodedLen};
use frame::{
	deps::{
		sp_io::hashing::blake2_256,
		sp_runtime::traits::{One, TrailingZeroInput},
	},
	traits::BlockNumberProvider,
};
use frame_support::{
	storage_alias,
	traits::{Currency, ReservableCurrency},
	Blake2_128Concat, BoundedVec, Twox64Concat,
};
use scale_info::TypeInfo;

/// Extended migration config for the migration.
pub trait MigrationConfig: crate::pallet::Config {
	/// The currency to unreserve deposits.
	type Currency: ReservableCurrency<Self::AccountId, Balance = crate::BalanceOf<Self>>;
}

/// Derive a multi-account ID from the sorted list of accounts and the threshold.
///
/// This is used to compute the inheritor for migrated recovery configs - the inheritor
/// will be the multisig account that the friends can control together.
///
/// NOTE: `who` must be sorted. If it is not, then you'll get the wrong answer.
pub fn multi_account_id<AccountId: Encode + Decode>(
	who: &[AccountId],
	threshold: u16,
) -> AccountId {
	let entropy = (b"modlpy/utilisuba", who, threshold).using_encoded(blake2_256);
	Decode::decode(&mut TrailingZeroInput::new(entropy.as_ref()))
		.expect("infinite length input; no invalid inputs for type; qed")
}

/// Balance type for v0 storage.
pub type BalanceOf<T> =
	<<T as MigrationConfig>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// Block number type from provider.
pub type BlockNumberFromProviderOf<T> =
	<<T as crate::pallet::Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

/// Friends bounded vec type.
pub type FriendsOf<T> = BoundedVec<
	<T as frame_system::Config>::AccountId,
	<T as crate::pallet::Config>::MaxFriendsPerConfig,
>;

/// Recovery configuration structure from v0.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Default, TypeInfo, MaxEncodedLen)]
pub struct RecoveryConfig<BlockNumber, Balance, Friends> {
	/// The minimum number of blocks since the start of the recovery process before the
	/// account can be recovered.
	pub delay_period: BlockNumber,
	/// The amount held in reserve of the `depositor`,
	/// to be returned once this configuration is removed.
	pub deposit: Balance,
	/// The list of friends which can help recover an account. Always sorted.
	pub friends: Friends,
	/// The number of approving friends needed to recover an account.
	pub threshold: u16,
}

impl<BlockNumber: Clone + Ord + One, Balance, Friends>
	RecoveryConfig<BlockNumber, Balance, Friends>
{
	/// Convert to a V1 `FriendGroup`.
	pub fn into_v1_friend_group<AccountId>(
		self,
		inheritor: AccountId,
	) -> crate::FriendGroup<BlockNumber, AccountId, Friends> {
		crate::FriendGroup {
			friends: self.friends,
			friends_needed: self.threshold as u32,
			inheritor,
			inheritance_delay: self.delay_period.clone(),
			inheritance_priority: 0,
			// At least one block delay to prevent mempool frontrunning
			cancel_delay: self.delay_period.max(One::one()),
		}
	}
}

/// Old active recovery structure from v0.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Default, TypeInfo, MaxEncodedLen)]
pub struct ActiveRecovery<BlockNumber, Balance, Friends> {
	/// The block number when the recovery process started.
	pub created: BlockNumber,
	/// The amount held in reserve of the `depositor`,
	/// to be returned once this recovery process is closed.
	pub deposit: Balance,
	/// The friends which have vouched so far. Always sorted.
	pub friends: Friends,
}

/// The set of recoverable accounts and their recovery configuration.
#[storage_alias]
pub type Recoverable<T: MigrationConfig> = StorageMap<
	crate::pallet::Pallet<T>,
	Twox64Concat,
	<T as frame_system::Config>::AccountId,
	RecoveryConfig<BlockNumberFromProviderOf<T>, BalanceOf<T>, FriendsOf<T>>,
>;

/// Old storage: Active recovery attempts.
///
/// First account is the account to be recovered, and the second account
/// is the user trying to recover the account.
#[storage_alias]
pub type ActiveRecoveries<T: MigrationConfig> = StorageDoubleMap<
	crate::pallet::Pallet<T>,
	Twox64Concat,
	<T as frame_system::Config>::AccountId,
	Twox64Concat,
	<T as frame_system::Config>::AccountId,
	ActiveRecovery<BlockNumberFromProviderOf<T>, BalanceOf<T>, FriendsOf<T>>,
>;

/// Old storage: The list of allowed proxy accounts.
///
/// Map from the user who can access it to the recovered account.
#[storage_alias]
pub type Proxy<T: MigrationConfig> = StorageMap<
	crate::pallet::Pallet<T>,
	Blake2_128Concat,
	<T as frame_system::Config>::AccountId,
	<T as frame_system::Config>::AccountId,
>;
