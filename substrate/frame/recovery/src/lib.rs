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

//! # Recovery Pallet
//!
//! Pallet Recovery allows you to have friends or family recover access to your account if you lose
//! your seed phrase or private key.
//!
//! ## Terminology
//!
//! - `lost`: An account that has lost access to its private key and needs to be recovered.
//! - `friend`: A befriended account that can approve a recovery process.
//! - `initiator`: An account that initiated a recovery attempt.
//! - `recovered`: An account that has been successfully recovered.
//! - `inheritor`: An account that is inheriting access to a lost account after recovery.
//! - `attempt`: An attempt to recover a lost account by an initiator.
//! - `priority`: The priority of a friend group in inheritance conflicts. See
//!   [`InheritancePriority`].
//! - `deposit`: An amount of currency that needs to be held for allocating on-chain storage.
//! - `friends_needed`: The number of friends that need to approve an attempt.
//! - `inheritance delay`: How long an attempt will be delayed before it can succeed.
//! - `provided block`: The blocks that are *provided* by the `T::BlockNumberProvider`.
//!
//! ## Scenario: Recovering a lost account
//!
//! Story of how the user Alice loses access and is recovered by her friends.
//!
//! 1. Alice uses the recovery pallet to configure one or more friends groups:
//!   - Alice picks a suitable `inheritor` account that will inherit the access to her account for
//!     each friend group. This could be a multisig.
//!   - Alice configures all groups via `set_friend_groups`.
//! 2. Alice loses access to her account and becomes a `lost` account.
//! 3. Any member (aka `initiator`) of Alice's friend groups become aware of the situation and
//!    starts a recovery `attempt` via `initiate_attempt`.
//! 4. The friend group self-organizes and one-by-one approve the ongoing attempt via
//!    `approve_attempt`.
//! 5. Exactly `friends_needed` friends approve the attempt (further approvals will fail since they
//!    are useless).
//! 6. Any account finishes the attempt via `finish_attempt` after at least *inheritance delay*
//!    blocks since the initiation have passed.
//! 7. Alice's account is now officially `recovered` and accessible by the `inheritor` account.
//! 8. The `inheritor` may call `control_inherited_account` at any point to transfer Alice's funds
//!    to her new account.
//!
//! ## Scenario: Multiple friend groups try to recover an account
//!
//! Alice may have configured multiple friend groups that all try to recover her account at the same
//! time. This can lead to a conflict of which friend group should eventually inherit the access.
//!
//! 1. Alice configures groups *Family* (delay 10d, priority 0) and *Friends* (delay 20d, priority
//!    1). Since numerical lower values denote higher priority, *Family* therefore has higher
//!    priority than *Friends*.
//! 1. Day 0: Alice loses access to her account.
//! 1. Day 6: *Friends* initiate a recovery attempt for Alice.
//! 1. Day 15: *Family* finally understands Polkadot and initiates an attempt as well.
//! 1. Day 25: *Family* inherits access to Alice account.
//! 1. Day 26: *Friends* group gets nothing since they have lower priority than *Family*.
//!
//! In the case above you see how the *Friends* group is now unable to recover Alice account since
//! the *Family* group already did it and has higher priority.
//! Now, imagine the case that the *Friends* group would have started on day 4 and would have
//! already recovered the account on day 24. Two days later, the *Family* group can take access back
//! and will replace the inheritor account with their own. The *Friends* group had access for two
//! days since they were faster.
//! If Alice account has most balance locked in 28 day staking this would not make a big difference,
//! since only the free balance would be immediately transferable.
//!
//! After a recovery attempt was completed, lower-priority friend groups cannot open a new attempt
//! to recover the account.
//!
//! ## Data Structures
//!
//! The pallet has three storage items, see the in-code docs [`FriendGroups`], [`Attempt`] and
//! [`Inheritor`]. Storage items may contain deposit "tickets" or similar noise and should therefore
//! not be read directly but only through the API.
//!
//! ## API
//!
//! *Reading* data can be done through the view functions:
//!
//! - `provided_block_number`: The block number that will be used to measure time.
//! - `friend_groups`: The friend groups of an account that can initiate recovery attempts.
//! - `attempts`: Ongoing recovery attempts for a lost account.
//! - `inheritor`: The account that inherited full access to the lost account.
//! - `inheritance`: All the recovered accounts that an account inherited access to.

#![recursion_limit = "1024"]
#![cfg_attr(not(feature = "std"), no_std)]
extern crate alloc;
use alloc::{boxed::Box, vec, vec::Vec};

use frame::{
	prelude::*,
	traits::{
		fungible::{hold::Balanced, Credit, Inspect, MutateHold},
		Consideration, Footprint, OnUnbalanced, OriginTrait,
	},
};
use types::{Bitfield, IdentifiedConsideration};

pub use pallet::*;
pub use weights::WeightInfo;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod migrations;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
pub mod types;
pub mod weights;

/// Maximum number of friend groups that an account can have.
pub const MAX_GROUPS_PER_ACCOUNT: u32 = 10;

pub type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;
pub type BalanceOf<T> = <<T as Config>::Currency as Inspect<AccountIdFor<T>>>::Balance;
pub type CreditOf<T> = Credit<AccountIdFor<T>, <T as Config>::Currency>;
/// The block number type that will be used to measure time.
pub type ProvidedBlockNumberOf<T> =
	<<T as Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

/// Friends of a friend group.
pub type FriendsOf<T> =
	BoundedVec<<T as frame_system::Config>::AccountId, <T as Config>::MaxFriendsPerConfig>;
pub type HashOf<T> = <T as frame_system::Config>::Hash;

/// Group of friends that can initiate a recovery attempt for a specific lost account.
#[derive(
	Clone,
	Eq,
	PartialEq,
	Encode,
	Decode,
	Default,
	Debug,
	TypeInfo,
	MaxEncodedLen,
	DecodeWithMemTracking,
)]
pub struct FriendGroup<ProvidedBlockNumber, AccountId, Friends> {
	/// List of friends that can initiate the recovery process. Always sorted.
	pub friends: Friends,

	/// The number of approving friends needed to recover an account.
	pub friends_needed: u32,

	/// The account that will inherit full access to the lost account upon successful recovery.
	pub inheritor: AccountId,

	/// Minimum time that a recovery attempt must stay active before it can be finished.
	///
	/// Uses a provided block number to avoid possible clock skew of parachains.
	pub inheritance_delay: ProvidedBlockNumber,

	/// Used to resolve inheritance conflicts when multiple friend groups finish a recovery.
	///
	/// Higher-priority friend groups can replace the inheritor of a lower-priority group. For
	/// example: you can set your family group as priority 0, your friends group as priority 1 and
	/// co-workers as priority 2. This in combination with the `inheritance_delay` enables you to
	/// ensure that the correct group receives the inheritance. See [`InheritancePriority`] for the
	/// numeric convention.
	pub inheritance_priority: InheritancePriority,

	/// The delay since the last approval of an attempt before the attempt can be canceled.
	///
	/// It ensures that a malicious recoverer does not abuse the `cancel_attempt` call to dodge an
	/// incoming slash from the lost account. They could otherwise monitor the TX pool and cancel
	/// the attempt just in time for the slash transaction to fail. Now instead, the lost account
	/// has at least `cancel_delay` provided blocks to slash the attempt.
	pub cancel_delay: ProvidedBlockNumber,
}

/// Index of a friend group of a lost account.
pub type FriendGroupIndex = u32;

/// Priority of a friend group in account inheritance conflicts.
///
/// Lower numerical values denote higher priority (so `0` is the strongest priority).
pub type InheritancePriority = u32;

/// A `FriendGroup` for a specific `Config`.
pub type FriendGroupOf<T> = FriendGroup<ProvidedBlockNumberOf<T>, AccountIdFor<T>, FriendsOf<T>>;

/// Collection of friend groups of a lost account.
pub type FriendGroupsOf<T> = BoundedVec<FriendGroupOf<T>, ConstU32<MAX_GROUPS_PER_ACCOUNT>>;

/// Approval bitfield for a specific number of friends.
pub type ApprovalBitfield<MaxFriends> = Bitfield<MaxFriends>;

/// Bitfield to track approval per friend in a friend group.
pub type ApprovalBitfieldOf<T> = ApprovalBitfield<<T as Config>::MaxFriendsPerConfig>;

/// An attempt to recover an account.
#[derive(
	Clone,
	Eq,
	PartialEq,
	Encode,
	Decode,
	Default,
	Debug,
	TypeInfo,
	MaxEncodedLen,
	DecodeWithMemTracking,
)]
pub struct Attempt<ProvidedBlockNumber, ApprovalBitfield, AccountId> {
	/// Index of the friend group that initiated the attempt.
	///
	/// This will never be more than `MAX_GROUPS_PER_ACCOUNT`.
	pub friend_group_index: FriendGroupIndex,

	/// The account that initiated the attempt.
	pub initiator: AccountId,

	/// The block number when the attempt was initiated.
	///
	/// Note that this can be a foreign (ie Relay) block number.
	pub init_block: ProvidedBlockNumber,

	/// The block number when the last friend approved the attempt.
	///
	/// Note that this can be a foreign (ie Relay) block number.
	pub last_approval_block: ProvidedBlockNumber,

	/// Bitfield tracking which friends approved.
	///
	/// Each bit corresponds to a friend in the `friend_group.friends` that has approved the
	/// attempt.
	pub approvals: ApprovalBitfield,
}

/// Attempt to recover an account.
pub type AttemptOf<T> = Attempt<ProvidedBlockNumberOf<T>, ApprovalBitfieldOf<T>, AccountIdFor<T>>;

/// Ticket for an attempt to recover an account.
pub type AttemptTicketOf<T> =
	IdentifiedConsideration<AccountIdFor<T>, Footprint, <T as Config>::AttemptConsideration>;

/// Ticket for the inheritor of an account.
pub type InheritorTicketOf<T> =
	IdentifiedConsideration<AccountIdFor<T>, Footprint, <T as Config>::InheritorConsideration>;

/// Amount of a security deposit - as opposed to a storage deposit.
pub type SecurityDepositOf<T> = BalanceOf<T>;

#[frame::pallet]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	#[pallet::storage_version(migrations::STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching call type.
		type RuntimeCall: Parameter
			+ Dispatchable<RuntimeOrigin = Self::RuntimeOrigin, PostInfo = PostDispatchInfo>
			+ GetDispatchInfo
			+ From<frame_system::Call<Self>>
			+ IsSubType<Call<Self>>
			+ IsType<<Self as frame_system::Config>::RuntimeCall>;

		/// The overarching hold reason.
		type RuntimeHoldReason: Parameter
			+ Member
			+ MaxEncodedLen
			+ Copy
			+ VariantCount
			+ From<HoldReason>;

		/// Query the block number that will be used to measure time.
		///
		/// Must return monotonically increasing values when called from consecutive blocks. Can be
		/// configured to return either:
		/// - the local block number of the runtime via `frame_system::Pallet`
		/// - a remote block number, eg from the relay chain through `RelaychainDataProvider`
		/// - an arbitrary value through a custom implementation of the trait
		///
		/// There is currently no migration provided to "hot-swap" block number providers and it may
		/// result in undefined behavior when doing so. Parachains are therefore best off setting
		/// this to their local block number provider if they have the pallet already deployed.
		///
		/// Suggested values:
		/// - Solo- and Relay-chains: `frame_system::Pallet`
		/// - Parachains that may produce blocks sparingly or only when needed (on-demand):
		///   - already have the pallet deployed: `frame_system::Pallet`
		///   - are freshly deploying this pallet: `RelaychainDataProvider`
		/// - Parachains with a reliably block production rate (PLO or bulk-coretime):
		///   - already have the pallet deployed: `frame_system::Pallet`
		///   - are freshly deploying this pallet: no strong recommendation. Both local and remote
		///     providers can be used. Relay provider can be a bit better in cases where the
		///     parachain is lagging its block production to avoid clock skew.
		type BlockNumberProvider: BlockNumberProvider;

		/// The currency mechanism.
		#[cfg(not(feature = "runtime-benchmarks"))]
		type Currency: MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>
			+ Balanced<Self::AccountId>;
		#[cfg(feature = "runtime-benchmarks")]
		type Currency: MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>
			+ Balanced<Self::AccountId>
			+ frame::traits::fungible::Mutate<Self::AccountId>;

		/// Storage consideration for holding friend group configs.
		type FriendGroupsConsideration: Consideration<Self::AccountId, Footprint>;

		/// Storage consideration for holding an attempt.
		type AttemptConsideration: Consideration<Self::AccountId, Footprint>;

		/// Storage consideration for holding an inheritor.
		type InheritorConsideration: Consideration<Self::AccountId, Footprint>;

		/// Security deposit taken for each attempt that the initiator needs to place.
		#[pallet::constant]
		type SecurityDeposit: Get<BalanceOf<Self>>;

		/// Handler for the `Credit` produced when a security deposit is slashed.
		///
		/// Use `()` to drop the credit and decrease total issuance (i.e. burn). Other common
		/// choices are a treasury sink or `pallet-dap`.
		type Slash: OnUnbalanced<CreditOf<Self>>;

		/// DO NOT REDUCE THIS VALUE. Maximum number of friends per account config.
		///
		/// Reducing this value can cause decoding errors in the bounded vectors.
		#[pallet::constant]
		type MaxFriendsPerConfig: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
	}

	/// The friend groups of an account that can conduct recovery attempts.
	///
	/// Modifying this storage is not possible while an account has ongoing recovery attempts.
	#[pallet::storage]
	pub type FriendGroups<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		(FriendGroupsOf<T>, T::FriendGroupsConsideration),
	>;

	/// Ongoing recovery attempts of a lost account indexed by `(lost, friend_group)`.
	#[pallet::storage]
	pub type Attempt<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		Blake2_128Concat,
		FriendGroupIndex,
		(AttemptOf<T>, AttemptTicketOf<T>, SecurityDepositOf<T>),
	>;

	/// The account that inherited full access to a lost account after successful recovery.
	///
	/// The key is the lost account and the value is the inheritor account.
	///
	/// NOTE: This could be a multisig or proxy account
	#[pallet::storage]
	pub type Inheritor<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		(InheritancePriority, T::AccountId, InheritorTicketOf<T>),
	>;

	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Deposit for configuring recovery friend groups.
		#[codec(index = 0)]
		FriendGroupsStorage,

		/// Deposit for an ongoing recovery attempt.
		#[codec(index = 1)]
		AttemptStorage,

		/// Deposit for the inheritor of a lost account.
		#[codec(index = 2)]
		InheritorStorage,

		/// Security deposit for a recovery attempt.
		#[codec(index = 3)]
		SecurityDeposit,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A recovery attempt was approved by a friend.
		AttemptApproved {
			lost: T::AccountId,
			friend_group_index: FriendGroupIndex,
			friend: T::AccountId,
		},
		/// A recovery attempt was canceled by either the lost account or the initiator.
		AttemptCanceled {
			lost: T::AccountId,
			friend_group_index: FriendGroupIndex,
			canceler: T::AccountId,
		},
		/// A recovery attempt was initiated by a friend.
		AttemptInitiated {
			lost: T::AccountId,
			friend_group_index: FriendGroupIndex,
			initiator: T::AccountId,
		},
		/// A recovery attempt was finished.
		AttemptFinished {
			lost: T::AccountId,
			friend_group_index: FriendGroupIndex,
			inheritor: T::AccountId,
			previous_inheritor: Option<T::AccountId>,
		},
		/// A recovery attempt was discarded because the account was already recovered by a
		/// friend group of equal or higher priority.
		///
		/// The attempt is consumed (removed from storage) and its deposits are released, but
		/// the existing inheritor remains unchanged.
		AttemptDiscarded {
			lost: T::AccountId,
			friend_group_index: FriendGroupIndex,
			existing_inheritor: T::AccountId,
		},
		/// A recovery attempt was slashed by the lost account.
		///
		/// The initiator will lose their security deposit.
		AttemptSlashed { lost: T::AccountId, friend_group_index: FriendGroupIndex },
		/// The friend groups of an account have been changed.
		FriendGroupsChanged { lost: T::AccountId },
		/// The inheritor of a lost account was revoked by the lost account.
		InheritorRevoked { lost: T::AccountId },
		/// A recovered account was controlled by its inheritor.
		///
		/// Check the `call_result` to see if it was successful.
		RecoveredAccountControlled {
			recovered: T::AccountId,
			inheritor: T::AccountId,
			call_hash: HashOf<T>,
			call_result: DispatchResult,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// This attempt is already fully approved and does not need any more votes.
		AlreadyApproved,
		/// The recovery attempt has already been initiated.
		AlreadyInitiated,
		/// The friend already voted for this attempt.
		AlreadyVoted,
		/// The lost account has ongoing recovery attempts.
		HasOngoingAttempts,
		/// The lost account cannot be a friend of itself.
		LostAccountInFriendGroup,
		/// The account was already recovered by a group of equal or higher priority.
		HigherPriorityRecovered,
		/// Cancel delay must be at least 1.
		NoCancelDelay,
		/// This account does not have any friend groups.
		NoFriendGroups,
		/// The friend group has no friends.
		NoFriends,
		/// The lost account does not have any inheritor.
		NoInheritor,
		/// Not enough friends approved this attempt.
		NotApproved,
		/// The referenced recovery attempt was not found.
		NotAttempt,
		/// The caller is not the initiator or the lost account.
		NotCanceller,
		/// The caller is not a friend of the lost account.
		NotFriend,
		/// A specific referenced friend group was not found.
		NotFriendGroup,
		/// The caller is not the inheritor of the lost account.
		NotInheritor,
		/// The cancel delay since the last approval or initialization has not yet passed.
		NotYetCancelable,
		/// The inheritance delay of this attempt has not yet passed.
		NotYetInheritable,
		/// Too many friend groups.
		TooManyFriendGroups,
		/// The number of friends needed is greater than the number of friends.
		TooManyFriendsNeeded,
		/// The number of friends needed is zero.
		NoFriendsNeeded,
		/// The friends of a friend group are not sorted or not unique.
		FriendsNotSortedOrUnique,
		/// Two friend groups have the same set of friends.
		DuplicateFriendGroups,
	}

	#[pallet::view_functions]
	impl<T: Config> Pallet<T> {
		/// The provided block number that will be used to measure time.
		pub fn provided_block_number() -> ProvidedBlockNumberOf<T> {
			T::BlockNumberProvider::current_block_number()
		}

		/// The friend groups of an account that can initiate recovery attempts.
		pub fn friend_groups(lost: T::AccountId) -> Vec<FriendGroupOf<T>> {
			FriendGroups::<T>::get(lost).map(|(g, _t)| g.into_inner()).unwrap_or_default()
		}

		/// Ongoing recovery attempts for a lost account.
		pub fn attempts(lost: T::AccountId) -> Vec<(FriendGroupOf<T>, AttemptOf<T>)> {
			Attempt::<T>::iter_prefix(&lost)
				.filter_map(|(friend_group_index, (attempt, _ticket, _deposit))| {
					let friend_group = Self::friend_group_of(&lost, friend_group_index).ok()?;
					Some((friend_group, attempt))
				})
				.collect()
		}

		/// The account that inherited full access to the lost account.
		pub fn inheritor(lost: T::AccountId) -> Option<T::AccountId> {
			Inheritor::<T>::get(lost).map(|(_, inheritor, _)| inheritor)
		}

		/// All the recovered accounts that `heir` inherited access to.
		pub fn inheritance(heir: T::AccountId) -> Vec<T::AccountId> {
			let mut inheritance = Vec::new();

			for (recovered, (_, inheritor, _)) in Inheritor::<T>::iter() {
				if inheritor != heir {
					continue;
				}
				let Err(pos) = inheritance.binary_search(&recovered) else { continue };

				inheritance.insert(pos, recovered);
			}

			inheritance
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Allows the inheritor of a recovered account to control it.
		///
		/// The controller is not allowed to dispatch calls of the recovery pallet. Otherwise they
		/// could mess with the recovery configuration and possibly cancel or slash attempts from
		/// higher-priority friend groups.
		#[pallet::call_index(0)]
		#[pallet::weight({
			let di = call.get_dispatch_info();
			(T::WeightInfo::control_inherited_account().saturating_add(di.call_weight), di.class)
		})]
		pub fn control_inherited_account(
			origin: OriginFor<T>,
			recovered: AccountIdLookupOf<T>,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResult {
			let maybe_inheritor = ensure_signed(origin)?;
			let recovered = T::Lookup::lookup(recovered)?;

			let inheritor = Inheritor::<T>::get(&recovered)
				.map(|(_, inheritor, _ticket)| inheritor)
				.ok_or(Error::<T>::NoInheritor)?;
			ensure!(maybe_inheritor == inheritor, Error::<T>::NotInheritor);

			let mut origin: T::RuntimeOrigin =
				frame_system::RawOrigin::Signed(recovered.clone()).into();
			// Reentrancy guard
			origin.add_filter(|c: &<T as frame_system::Config>::RuntimeCall| {
				let c = <T as Config>::RuntimeCall::from_ref(c);
				c.is_sub_type().is_none()
			});

			let call_hash = call.using_encoded(&T::Hashing::hash);
			let call_result = call.dispatch(origin).map(|_| ()).map_err(|r| r.error);

			Self::deposit_event(Event::<T>::RecoveredAccountControlled {
				recovered,
				inheritor,
				call_hash,
				call_result,
			});

			// NOTE: We ALWAYS return okay if the caller had the permission to control the lost
			// account regardless of the inner call result.
			Ok(())
		}

		/// Revoke the inheritor of the calling (lost) account.
		///
		/// This removes the inheritor entry and refunds the inheritor deposit. Can only be called
		/// by the lost account itself after it regains access.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::revoke_inheritor())]
		pub fn revoke_inheritor(origin: OriginFor<T>) -> DispatchResult {
			let lost = ensure_signed(origin)?;

			let (_priority, _inheritor, ticket) =
				Inheritor::<T>::take(&lost).ok_or(Error::<T>::NoInheritor)?;

			let _: Result<(), DispatchError> = ticket.try_drop().defensive();

			Self::deposit_event(Event::<T>::InheritorRevoked { lost });

			Ok(())
		}

		/// Set the friend groups of the calling account before it lost access.
		///
		/// Cannot be used while there are ongoing recovery attempts. The friends of each group
		/// MUST be sorted and unique. Trying to insert two friend groups with the same set of
		/// friends will result in an error.
		///
		/// A `FriendGroupsChanged` event is emitted only when the new friends groups differed from
		/// the old ones.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::set_friend_groups())]
		pub fn set_friend_groups(
			origin: OriginFor<T>,
			friend_groups: Vec<FriendGroupOf<T>>,
		) -> DispatchResult {
			let lost = ensure_signed(origin)?;

			if Attempt::<T>::iter_prefix(&lost).next().is_some() {
				return Err(Error::<T>::HasOngoingAttempts.into());
			}

			let (old_friend_groups, old_ticket) = match FriendGroups::<T>::get(&lost) {
				Some((g, t)) => (g, Some(t)),
				None => Default::default(),
			};

			let new_friend_groups = Self::bound_friend_groups(&lost, friend_groups)?;

			// Easy case where all are removed:
			if new_friend_groups.is_empty() {
				if let Some(old_ticket) = old_ticket {
					old_ticket.drop(&lost)?;
				}
				FriendGroups::<T>::remove(&lost);
				if !old_friend_groups.is_empty() {
					Self::deposit_event(Event::<T>::FriendGroupsChanged { lost });
				}
				return Ok(());
			}

			let new_footprint = Self::friend_group_footprint(&new_friend_groups);
			let new_ticket = if let Some(old_ticket) = old_ticket {
				old_ticket.update(&lost, new_footprint)?
			} else {
				T::FriendGroupsConsideration::new(&lost, new_footprint)?
			};
			FriendGroups::<T>::insert(&lost, (&new_friend_groups, &new_ticket));

			if new_friend_groups != old_friend_groups {
				Self::deposit_event(Event::<T>::FriendGroupsChanged { lost });
			}

			Ok(())
		}

		/// Attempt to recover a lost account by a friend within the given friend group.
		///
		/// The initiator's approval is recorded automatically, so they do not need to call
		/// `approve_attempt` themselves.
		///
		/// Once an account has been recovered by a friend group, no friend group of equal or lower
		/// priority can open a new attempt: it will fail with [`Error::HigherPriorityRecovered`].
		/// Only a strictly higher-priority group (lower numerical
		/// [`FriendGroup::inheritance_priority`]) can take over the inheritor.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::initiate_attempt())]
		pub fn initiate_attempt(
			origin: OriginFor<T>,
			lost: AccountIdLookupOf<T>,
			friend_group_index: FriendGroupIndex,
		) -> DispatchResult {
			let initiator = ensure_signed(origin)?;
			let lost = T::Lookup::lookup(lost)?;

			if Self::attempt_of(&lost, friend_group_index).is_ok() {
				return Err(Error::<T>::AlreadyInitiated.into());
			}

			let friend_group = Self::friend_group_of(&lost, friend_group_index)?;
			let initiator_index = friend_group
				.friends
				.iter()
				.position(|f| f == &initiator)
				.ok_or(Error::<T>::NotFriend)?;

			if let Some((inheritance_priority, _, _)) = Inheritor::<T>::get(&lost) {
				ensure!(
					friend_group.inheritance_priority < inheritance_priority,
					Error::<T>::HigherPriorityRecovered
				);
			}

			// The initiator counts as the first approval, so they don't have to sign twice.
			let approvals = ApprovalBitfield::default()
				.with_bits([initiator_index])
				.defensive_proof("initiator_index < friends.len() <= MaxFriendsPerConfig; qed")
				.unwrap_or_default();

			let now = T::BlockNumberProvider::current_block_number();
			let attempt = AttemptOf::<T> {
				friend_group_index,
				initiator: initiator.clone(),
				init_block: now,
				last_approval_block: now,
				approvals,
			};

			let deposit = T::SecurityDeposit::get();
			let () = T::Currency::hold(&HoldReason::SecurityDeposit.into(), &initiator, deposit)?;

			let ticket = AttemptTicketOf::<T>::new(&initiator, Self::attempt_footprint())?;
			Attempt::<T>::insert(&lost, friend_group_index, (&attempt, &ticket, &deposit));

			Self::deposit_event(Event::<T>::AttemptInitiated {
				lost: lost.clone(),
				friend_group_index,
				initiator: initiator.clone(),
			});
			Self::deposit_event(Event::<T>::AttemptApproved {
				lost,
				friend_group_index,
				friend: initiator,
			});

			Ok(())
		}

		/// Approve the recovery for a lost account.
		///
		/// Must be called by a friend of the friend group that the recovery attempt belongs to that
		/// did not yet vote. Voting is only allowed until the threshold is reached.
		/// `finish_attempt` should be called after the last friend voted.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::approve_attempt())]
		pub fn approve_attempt(
			origin: OriginFor<T>,
			lost: AccountIdLookupOf<T>,
			friend_group_index: FriendGroupIndex,
		) -> DispatchResult {
			let friend = ensure_signed(origin)?;
			let lost = T::Lookup::lookup(lost)?;
			let now = T::BlockNumberProvider::current_block_number();

			let (mut attempt, ticket, deposit) = Self::attempt_of(&lost, friend_group_index)?;
			let friend_group = Self::friend_group_of(&lost, friend_group_index).defensive()?;

			let friend_index = friend_group
				.friends
				.iter()
				.position(|f| f == &friend)
				.ok_or(Error::<T>::NotFriend)?;

			let friends_voted = attempt.approvals.count_ones();
			ensure!(friends_voted < friend_group.friends_needed, Error::<T>::AlreadyApproved);
			attempt.last_approval_block = now;

			attempt
				.approvals
				.set_if_not_set(friend_index)
				.map_err(|_| Error::<T>::AlreadyVoted)?;

			// NOTE: We do not update the ticket since the attempt has static size.
			Attempt::<T>::insert(&lost, friend_group_index, (&attempt, &ticket, &deposit));

			Self::deposit_event(Event::<T>::AttemptApproved { lost, friend_group_index, friend });

			Ok(())
		}

		/// Finish a recovery attempt and make the lost account accessible from the inheritor.
		///
		/// Can be called by anyone who is willing to pay for the inheritor deposit.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::finish_attempt())]
		pub fn finish_attempt(
			origin: OriginFor<T>,
			lost: AccountIdLookupOf<T>,
			friend_group_index: FriendGroupIndex,
		) -> DispatchResult {
			let caller = ensure_signed(origin)?;
			let lost = T::Lookup::lookup(lost)?;
			let now = T::BlockNumberProvider::current_block_number();

			let (attempt, attempts_ticket, deposit) =
				Attempt::<T>::take(&lost, &friend_group_index).ok_or(Error::<T>::NotAttempt)?;

			// We NEVER block a recovery on a buggy initiator account.
			let _: Result<(), DispatchError> = attempts_ticket.try_drop().defensive();
			let _: Result<BalanceOf<T>, DispatchError> = T::Currency::release(
				&HoldReason::SecurityDeposit.into(),
				&attempt.initiator,
				deposit,
				Precision::BestEffort,
			)
			.defensive();

			let friend_group = Self::friend_group_of(&lost, friend_group_index).defensive()?;

			// Check if the attempt is now complete
			let approvals = attempt.approvals.count_ones();
			ensure!(
				// We use >= defensively, but it should be at most ==
				approvals >= friend_group.friends_needed,
				Error::<T>::NotApproved
			);

			let inheritable_at = attempt
				.init_block
				.checked_add(&friend_group.inheritance_delay)
				.ok_or(ArithmeticError::Overflow)?;
			ensure!(now >= inheritable_at, Error::<T>::NotYetInheritable);
			// NOTE: We dont need to check the cancel delay, since enough friends voted and we dont
			// assume fully malicious behavior.

			let inheritor = friend_group.inheritor;
			let inheritance_priority = friend_group.inheritance_priority;

			match Inheritor::<T>::get(&lost) {
				None => {
					let ticket = Self::inheritor_ticket(&caller)?;
					Inheritor::<T>::insert(&lost, (inheritance_priority, &inheritor, ticket));
					Self::deposit_event(Event::<T>::AttemptFinished {
						lost,
						friend_group_index,
						inheritor,
						previous_inheritor: None,
					});
				},
				// new recovery has a higher priority, we replace the existing inheritor
				Some((old_priority, old_inheritor, ticket))
					if inheritance_priority < old_priority =>
				{
					let ticket = ticket.update(&caller, Self::inheritor_footprint())?;
					Inheritor::<T>::insert(&lost, (inheritance_priority, &inheritor, ticket));
					Self::deposit_event(Event::<T>::AttemptFinished {
						lost,
						friend_group_index,
						inheritor,
						previous_inheritor: Some(old_inheritor),
					});
				},
				Some((_, existing_inheritor, _)) => {
					// The existing inheritor stays since an equal or higher priority group
					// already recovered the account.
					Self::deposit_event(Event::<T>::AttemptDiscarded {
						lost,
						friend_group_index,
						existing_inheritor,
					});
				},
			};

			Ok(())
		}

		/// The lost account can cancel an attempt at any moment; the initiator, only after a delay.
		///
		/// This will release the security deposit back to the initiator. The cancel delay must be
		/// respected if the initiator calls it to prevent it from front-running the lost account
		/// from slashing the attempt.
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::cancel_attempt())]
		pub fn cancel_attempt(
			origin: OriginFor<T>,
			lost: AccountIdLookupOf<T>,
			friend_group_index: FriendGroupIndex,
		) -> DispatchResult {
			let canceler = ensure_signed(origin)?;
			let lost = T::Lookup::lookup(lost)?;
			let now = T::BlockNumberProvider::current_block_number();

			let (attempt, ticket, deposit) =
				Attempt::<T>::take(&lost, &friend_group_index).ok_or(Error::<T>::NotAttempt)?;

			ensure!(canceler == attempt.initiator || canceler == lost, Error::<T>::NotCanceller);

			// Ignore the return value since we always want to allow to cancel an attempt.
			let _ignored = ticket.try_drop().defensive();
			let _: Result<BalanceOf<T>, DispatchError> = T::Currency::release(
				&HoldReason::SecurityDeposit.into(),
				&attempt.initiator,
				deposit,
				Precision::BestEffort,
			)
			.defensive();

			let friend_group = Self::friend_group_of(&lost, friend_group_index).defensive()?;

			if canceler != lost {
				let cancelable_at = attempt
					.last_approval_block
					.checked_add(&friend_group.cancel_delay)
					.ok_or(ArithmeticError::Overflow)?;
				ensure!(now >= cancelable_at, Error::<T>::NotYetCancelable);
			}
			// NOTE: It is possible to cancel a fully approved attempt, but since we check the
			// cancel delay, we ensure that every friend had enough time to call
			// `finish_attempt`.

			Self::deposit_event(Event::<T>::AttemptCanceled { lost, friend_group_index, canceler });

			Ok(())
		}

		/// Slash a malicious recovery attempt and burn the security deposit of the initiator.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::slash_attempt())]
		pub fn slash_attempt(
			origin: OriginFor<T>,
			friend_group_index: FriendGroupIndex,
		) -> DispatchResult {
			let lost = ensure_signed(origin)?;

			let (attempt, ticket, deposit) =
				Attempt::<T>::take(&lost, &friend_group_index).ok_or(Error::<T>::NotAttempt)?;

			let _: Result<(), DispatchError> = ticket.try_drop().defensive();
			Self::handle_slash(&attempt.initiator, deposit);

			Self::deposit_event(Event::<T>::AttemptSlashed { lost, friend_group_index });

			Ok(())
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			assert!(
				T::MaxFriendsPerConfig::get() > 0,
				"MaxFriendsPerConfig must be greater than 0"
			);

			let bitfield = ApprovalBitfieldOf::<T>::default();
			assert!(bitfield.0.len() >= 1, "Default works");
		}
	}
}

impl<T: Config> Pallet<T> {
	pub fn friend_group_footprint(friend_groups: &FriendGroupsOf<T>) -> Footprint {
		if friend_groups.is_empty() {
			defensive!("Do not call with empty friend groups");
		}

		Footprint::from_encodable(friend_groups)
	}

	pub fn attempt_footprint() -> Footprint {
		Footprint::from_mel::<AttemptOf<T>>()
	}

	pub fn inheritor_footprint() -> Footprint {
		Footprint::from_mel::<(InheritancePriority, T::AccountId)>()
	}

	pub fn inheritor_ticket(who: &T::AccountId) -> Result<InheritorTicketOf<T>, DispatchError> {
		InheritorTicketOf::<T>::new(&who, Self::inheritor_footprint())
	}

	pub fn friend_group_of(
		lost: &T::AccountId,
		friend_group_index: FriendGroupIndex,
	) -> Result<FriendGroupOf<T>, Error<T>> {
		let friend_groups = match FriendGroups::<T>::get(lost) {
			Some((g, _t)) => g,
			None => return Err(Error::<T>::NoFriendGroups),
		};
		friend_groups
			.get(friend_group_index as usize)
			.cloned()
			.ok_or(Error::<T>::NotFriendGroup)
	}

	pub fn attempt_of(
		lost: &T::AccountId,
		friend_group_index: FriendGroupIndex,
	) -> Result<(AttemptOf<T>, AttemptTicketOf<T>, SecurityDepositOf<T>), Error<T>> {
		pallet::Attempt::<T>::get(lost, friend_group_index).ok_or(Error::<T>::NotAttempt)
	}

	/// Sanity check the friend groups and bound them into a bounded vector.
	pub fn bound_friend_groups(
		lost: &T::AccountId,
		mut friend_groups: Vec<FriendGroupOf<T>>,
	) -> Result<FriendGroupsOf<T>, Error<T>> {
		for friend_group in &mut friend_groups {
			ensure!(!friend_group.friends.is_empty(), Error::<T>::NoFriends);
			// cannot contain the lost account itself
			ensure!(!friend_group.friends.contains(&lost), Error::<T>::LostAccountInFriendGroup);
			ensure!(
				friend_group.friends.windows(2).all(|w| w[0] < w[1]),
				Error::<T>::FriendsNotSortedOrUnique
			);
			ensure!(
				friend_group.friends_needed as usize <= friend_group.friends.len(),
				Error::<T>::TooManyFriendsNeeded
			);
			ensure!(friend_group.friends_needed > 0, Error::<T>::NoFriendsNeeded);
			// prevent mempool frontrunning by requiring at least 1 block
			ensure!(!friend_group.cancel_delay.is_zero(), Error::<T>::NoCancelDelay);
		}

		for (i, group_a) in friend_groups.iter().enumerate() {
			for group_b in friend_groups.iter().skip(i + 1) {
				ensure!(group_a.friends != group_b.friends, Error::<T>::DuplicateFriendGroups);
			}
		}

		friend_groups.try_into().map_err(|_| Error::<T>::TooManyFriendGroups)
	}

	/// Slash a security deposit and hand the resulting `Credit` to `T::Slash`.
	fn handle_slash(who: &T::AccountId, amount: SecurityDepositOf<T>) {
		let (credit, missing) =
			T::Currency::slash(&HoldReason::SecurityDeposit.into(), who, amount);
		if !missing.is_zero() {
			defensive!("could not slash full security deposit");
		}
		T::Slash::on_unbalanced(credit);
	}
}
