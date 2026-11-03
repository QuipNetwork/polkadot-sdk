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

//! > Made with *Substrate*, for *Polkadot*.
//!
//! [![github]](https://github.com/paritytech/polkadot-sdk/tree/master/substrate/frame/bags-list) -
//! [![polkadot]](https://polkadot.com)
//!
//! [polkadot]:
//!     https://img.shields.io/badge/polkadot-E6007A?style=for-the-badge&logo=polkadot&logoColor=white
//! [github]:
//!     https://img.shields.io/badge/github-8da0cb?style=for-the-badge&labelColor=555555&logo=github
//!
//!  # Bags-List Pallet
//!
//! An onchain implementation of a semi-sorted linked list, with permissionless sorting and update
//! operations.
//!
//! ## Pallet API
//!
//! See the [`pallet`] module for more information about the interfaces this pallet exposes,
//! including its configuration trait, dispatchables, storage items, events and errors.
//!
//! This pallet provides an implementation of
//! [`frame_election_provider_support::SortedListProvider`] and it can typically be used by another
//! pallet via this API.
//!
//! ## Overview
//!
//! This pallet splits `AccountId`s into different bags. Within a bag, these `AccountId`s are stored
//! as nodes in a linked-list manner. This pallet then provides iteration over all bags, which
//! basically allows an infinitely large list of items to be kept in a sorted manner.
//!
//! Each bags has a upper and lower range of scores, denoted by [`Config::BagThresholds`]. All nodes
//! within a bag must be within the range of the bag. If not, the permissionless [`Pallet::rebag`]
//! can be used to move any node to the right bag.
//!
//! Once a `rebag` happens, the order within a node is still not enforced. To move a node to the
//! optimal position in a bag, the [`Pallet::put_in_front_of`] or [`Pallet::put_in_front_of_other`]
//! can be used.
//!
//! ## Examples
//!
//! See [`example`] for a diagram of `rebag` and `put_in_front_of` operations.
//!
//! ## Low Level / Implementation Details
//!
//! The data structure exposed by this pallet aims to be optimized for:
//!
//! - insertions and removals.
//! - iteration over the top* N items by score, where the precise ordering of items doesn't
//!   particularly matter.
//!
//! ### Further Details
//!
//! - items are kept in bags, which are delineated by their range of score (See
//!   [`Config::BagThresholds`]).
//! - for iteration, bags are chained together from highest to lowest and elements within the bag
//!   are iterated from head to tail.
//! - items within a bag are iterated in order of insertion. Thus removing an item and re-inserting
//!   it will worsen its position in list iteration; this reduces incentives for some types of spam
//!   that involve consistently removing and inserting for better position. Further, ordering
//!   granularity is thus dictated by range between each bag threshold.
//! - if an item's score changes to a value no longer within the range of its current bag the item's
//!   position will need to be updated by an external actor with rebag (update), or removal and
//!   insertion.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(doc)]
#[cfg_attr(doc, aquamarine::aquamarine)]
/// In this example, assuming each node has an equal id and score (eg. node 21 has a score of 21),
/// the node 22 can be moved from bag 1 to bag 0 with the `rebag` operation.
///
/// Once the whole list is iterated, assuming the above above rebag happens, the order of iteration
/// would be: `25, 21, 22, 12, 22, 5, 7, 3`.
///
/// Moreover, in bag2, node 7 can be moved to the front of node 5 with the `put_in_front_of`, as it
/// has a higher score.
///
/// ```mermaid
/// graph LR
/// 	Bag0 --> Bag1 --> Bag2
///
/// 	subgraph Bag0[Bag 0: 21-30 DOT]
/// 		direction LR
/// 		25 --> 21 --> 22X[22]
/// 	end
///
/// 	subgraph Bag1[Bag 1: 11-20 DOT]
/// 		direction LR
/// 		12 --> 22
/// 	end
///
/// 	subgraph Bag2[Bag 2: 0-10 DOT]
/// 		direction LR
/// 		5 --> 7 --> 3
/// 	end
///
/// 	style 22X stroke-dasharray: 5 5,opacity:50%
/// ```
///
/// The equivalent of this in code would be:
#[doc = docify::embed!("src/tests.rs", examples_work)]
pub mod example {}

use alloc::{boxed::Box, vec::Vec};
use codec::FullCodec;
use frame_election_provider_support::{ScoreProvider, SortedListProvider};
use frame_support::{
	traits::Get,
	weights::{Weight, WeightMeter},
};
use frame_system::ensure_signed;
use sp_runtime::traits::{AtLeast32BitUnsigned, Bounded, StaticLookup};

#[cfg(any(test, feature = "try-runtime", feature = "fuzz"))]
use sp_runtime::TryRuntimeError;

#[cfg(any(feature = "runtime-benchmarks", test))]
mod benchmarks;

pub mod list;
pub mod migrations;
#[cfg(any(test, feature = "fuzz"))]
pub mod mock;
#[cfg(test)]
mod tests;
pub mod weights;

pub use list::{notional_bag_for, Bag, List, ListError, Node};
pub use pallet::*;
pub use weights::WeightInfo;

pub(crate) const LOG_TARGET: &str = "runtime::bags-list";

// syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: crate::LOG_TARGET,
			concat!("[{:?}] 👜 [{}]", $patter),
			<frame_system::Pallet<T>>::block_number(),
			<crate::Pallet::<T, I> as frame_support::traits::PalletInfoAccess>::name()
			$(, $values)*
		)
	};
}

type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T, I = ()>(_);

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: weights::WeightInfo;

		/// Something that provides the scores of ids.
		type ScoreProvider: ScoreProvider<Self::AccountId, Score = Self::Score>;

		/// The list of thresholds separating the various bags.
		///
		/// Ids are separated into unsorted bags according to their score. This specifies the
		/// thresholds separating the bags. An id's bag is the largest bag for which the id's score
		/// is less than or equal to its upper threshold.
		///
		/// When ids are iterated, higher bags are iterated completely before lower bags. This means
		/// that iteration is _semi-sorted_: ids of higher score tend to come before ids of lower
		/// score, but peer ids within a particular bag are sorted in insertion order.
		///
		/// # Expressing the constant
		///
		/// This constant must be sorted in strictly increasing order. Duplicate items are not
		/// permitted.
		///
		/// There is an implied upper limit of `Score::MAX`; that value does not need to be
		/// specified within the bag. For any two threshold lists, if one ends with
		/// `Score::MAX`, the other one does not, and they are otherwise equal, the two
		/// lists will behave identically.
		///
		/// # Calculation
		///
		/// It is recommended to generate the set of thresholds in a geometric series, such that
		/// there exists some constant ratio such that `threshold[k + 1] == (threshold[k] *
		/// constant_ratio).max(threshold[k] + 1)` for all `k`.
		///
		/// The helpers in the `/utils/frame/generate-bags` module can simplify this calculation.
		///
		/// # Examples
		///
		/// - If `BagThresholds::get().is_empty()`, then all ids are put into the same bag, and
		///   iteration is strictly in insertion order.
		/// - If `BagThresholds::get().len() == 64`, and the thresholds are determined according to
		///   the procedure given above, then the constant ratio is equal to 2.
		/// - If `BagThresholds::get().len() == 200`, and the thresholds are determined according to
		///   the procedure given above, then the constant ratio is approximately equal to 1.248.
		/// - If the threshold list begins `[1, 2, 3, ...]`, then an id with score 0 or 1 will fall
		///   into bag 0, an id with score 2 will fall into bag 1, etc.
		///
		/// # Migration
		///
		/// In the event that this list ever changes, a copy of the old bags list must be retained.
		/// With that `List::migrate` can be called, which will perform the appropriate migration.
		#[pallet::constant]
		type BagThresholds: Get<&'static [Self::Score]>;

		/// Maximum number of accounts that may be re-bagged automatically in `on_idle`.
		///
		/// A value of `0` (obtained by configuring `type MaxAutoRebagPerBlock = ();`) disables
		/// the feature.
		#[pallet::constant]
		type MaxAutoRebagPerBlock: Get<u32>;

		/// The type used to dictate a node position relative to other nodes.
		type Score: Clone
			+ Default
			+ PartialEq
			+ Eq
			+ Ord
			+ PartialOrd
			+ core::fmt::Debug
			+ Copy
			+ AtLeast32BitUnsigned
			+ Bounded
			+ TypeInfo
			+ FullCodec
			+ MaxEncodedLen;
	}

	/// A single node, within some bag.
	///
	/// Nodes store links forward and back within their respective bags.
	#[pallet::storage]
	pub type ListNodes<T: Config<I>, I: 'static = ()> =
		CountedStorageMap<_, Twox64Concat, T::AccountId, list::Node<T, I>>;

	/// A bag stored in storage.
	///
	/// Stores a `Bag` struct, which stores head and tail pointers to itself.
	#[pallet::storage]
	pub type ListBags<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, T::Score, list::Bag<T, I>>;

	/// Pointer that remembers the next node that will be auto-rebagged.
	/// When `None`, the next scan will start from the list head again.
	#[pallet::storage]
	pub type NextNodeAutoRebagged<T: Config<I>, I: 'static = ()> =
		StorageValue<_, T::AccountId, OptionQuery>;

	/// Lock all updates to this pallet.
	///
	/// If any nodes needs updating, removal or addition due to a temporary lock, the
	/// [`Call::rebag`] can be used.
	#[pallet::storage]
	pub type Lock<T: Config<I>, I: 'static = ()> = StorageValue<_, (), OptionQuery>;

	/// Accounts that failed to be inserted into the bags-list due to locking.
	/// These accounts will be processed with priority in `on_idle` or via `rebag` extrinsic.
	///
	/// Note: This storage is intentionally unbounded. The following factors make bounding
	/// unnecessary:
	/// 1. The storage usage is temporary - accounts are processed and removed in `on_idle`
	/// 2. The pallet is only locked during snapshot generation, which is weight-limited
	/// 3. Processing happens at multiple accounts per block, clearing even large backlogs quickly
	/// 4. An artificial limit could be exhausted by an attacker, preventing legitimate
	///    auto-rebagging from putting accounts in the correct position
	///
	/// We don't store the score here - it's always fetched from `ScoreProvider` when processing,
	/// ensuring we use the most up-to-date score (accounts may have been slashed, rewarded, etc.
	/// while waiting in the queue).
	#[pallet::storage]
	pub type PendingRebag<T: Config<I>, I: 'static = ()> =
		CountedStorageMap<_, Twox64Concat, T::AccountId, ()>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// Moved an account from one bag to another.
		Rebagged { who: T::AccountId, from: T::Score, to: T::Score },
		/// Updated the score of some account to the given amount.
		ScoreUpdated { who: T::AccountId, new_score: T::Score },
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// A error in the list interface implementation.
		List(ListError),
		/// Could not update a node, because the pallet is locked.
		Locked,
	}

	impl<T, I> From<ListError> for Error<T, I> {
		fn from(t: ListError) -> Self {
			Error::<T, I>::List(t)
		}
	}

	#[pallet::view_functions]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Get the current `score` of a given account.
		///
		/// Returns `(current, real_score)`, the former being the current score that this pallet is
		/// aware of, which may or may not be up to date, and the latter being the real score, as
		/// provided by
		// [`Config::ScoreProvider`].
		/// If the two differ, it means this node is eligible for [`Call::rebag`].
		pub fn scores(who: T::AccountId) -> (Option<T::Score>, Option<T::Score>) {
			(ListNodes::<T, I>::get(&who).map(|node| node.score), T::ScoreProvider::score(&who))
		}
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Declare that some `dislocated` account has, through rewards or penalties, sufficiently
		/// changed its score that it should properly fall into a different bag than its current
		/// one.
		///
		/// Anyone can call this function about any potentially dislocated account.
		///
		/// Will always update the stored score of `dislocated` to the correct score, based on
		/// `ScoreProvider`.
		///
		/// If `dislocated` does not exists, it returns an error.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::rebag_non_terminal().max(T::WeightInfo::rebag_terminal()))]
		pub fn rebag(origin: OriginFor<T>, dislocated: AccountIdLookupOf<T>) -> DispatchResult {
			ensure_signed(origin)?;
			let dislocated = T::Lookup::lookup(dislocated)?;
			Self::ensure_unlocked().map_err(|_| Error::<T, I>::Locked)?;

			Self::rebag_internal(&dislocated).map_err::<DispatchError, _>(Into::into)?;

			Ok(())
		}

		/// Move the caller's Id directly in front of `lighter`.
		///
		/// The dispatch origin for this call must be _Signed_ and can only be called by the Id of
		/// the account going in front of `lighter`. Fee is payed by the origin under all
		/// circumstances.
		///
		/// Only works if:
		///
		/// - both nodes are within the same bag,
		/// - and `origin` has a greater `Score` than `lighter`.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::put_in_front_of())]
		pub fn put_in_front_of(
			origin: OriginFor<T>,
			lighter: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let heavier = ensure_signed(origin)?;
			let lighter = T::Lookup::lookup(lighter)?;
			Self::ensure_unlocked().map_err(|_| Error::<T, I>::Locked)?;
			List::<T, I>::put_in_front_of(&lighter, &heavier)
				.map_err::<Error<T, I>, _>(Into::into)
				.map_err::<DispatchError, _>(Into::into)
		}

		/// Same as [`Pallet::put_in_front_of`], but it can be called by anyone.
		///
		/// Fee is paid by the origin under all circumstances.
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::put_in_front_of())]
		pub fn put_in_front_of_other(
			origin: OriginFor<T>,
			heavier: AccountIdLookupOf<T>,
			lighter: AccountIdLookupOf<T>,
		) -> DispatchResult {
			ensure_signed(origin)?;
			let lighter = T::Lookup::lookup(lighter)?;
			let heavier = T::Lookup::lookup(heavier)?;
			Self::ensure_unlocked().map_err(|_| Error::<T, I>::Locked)?;
			List::<T, I>::put_in_front_of(&lighter, &heavier)
				.map_err::<Error<T, I>, _>(Into::into)
				.map_err::<DispatchError, _>(Into::into)
		}
	}

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		fn integrity_test() {
			// to ensure they are strictly increasing, this also implies that duplicates are
			// detected.
			assert!(
				T::BagThresholds::get().windows(2).all(|window| window[1] > window[0]),
				"thresholds must strictly increase, and have no duplicates",
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_: BlockNumberFor<T>) -> Result<(), TryRuntimeError> {
			<Self as SortedListProvider<T::AccountId>>::try_state()
		}

		/// Called during the idle phase of block execution.
		/// Automatically performs a limited number of `rebag` operations each block,
		/// incrementally correcting the position of accounts within the bags-list.
		///
		/// Processes accounts in the following priority order:
		/// 1. Pending accounts that failed to be inserted due to locking
		/// 2. Regular accounts that need rebagging
		///
		/// Guarantees processing as many nodes as possible without failing on errors.
		/// It stores a persistent cursor to continue across blocks.
		fn on_idle(_n: BlockNumberFor<T>, limit: Weight) -> Weight {
			let mut meter = WeightMeter::with_limit(limit);
			let per_item = T::WeightInfo::on_idle_rebag();

			let rebag_budget = T::MaxAutoRebagPerBlock::get();
			if rebag_budget == 0 {
				log!(debug, "Auto-rebag skipped: rebag_budget=0");
				return meter.consumed();
			}

			// Fixed overhead for storage ops outside the per-item loop:
			// counter reads (ListNodes, PendingRebag), Lock read, cursor read
			// (NextNodeAutoRebagged), cursor-update contains_key read, and cursor write.
			let overhead = T::DbWeight::get().reads_writes(5, 1);

			// Early exit: not enough weight for overhead + at least one rebag.
			if meter.try_consume(overhead.saturating_add(per_item)).is_err() {
				return meter.consumed();
			}

			let total_nodes = ListNodes::<T, I>::count();
			let pending_count = PendingRebag::<T, I>::count();

			if total_nodes == 0 && pending_count == 0 {
				log!(debug, "Auto-rebag skipped: total_nodes=0 and pending_count=0");
				return meter.consumed();
			}

			if Self::ensure_unlocked().is_err() {
				log!(debug, "Auto-rebag skipped: pallet is locked");
				return meter.consumed();
			}

			log!(
				debug,
				"Starting auto-rebag. Budget: {} accounts/block, total_nodes={}, pending_count={}.",
				rebag_budget,
				total_nodes,
				pending_count
			);

			let cursor = NextNodeAutoRebagged::<T, I>::get();
			let regular_iter = match cursor {
				Some(ref last) => {
					log!(debug, "Next node from previous block: {:?}", last);

					// Build an iterator that yields `last` first, then everything *after* it.
					let tail = Self::iter_from(last).unwrap_or_else(|_| Self::iter());
					let head_and_tail = core::iter::once(last.clone()).chain(tail);
					Box::new(head_and_tail) as Box<dyn Iterator<Item = T::AccountId>>
				},
				None => {
					log!(debug, "No NextNodeAutoRebagged found. Starting from head of the list");
					Self::iter()
				},
			};

			// Chain PendingRebag accounts with regular ListNodes.
			// PendingRebag comes first for priority processing
			let combined_iter = PendingRebag::<T, I>::iter_keys().chain(regular_iter);

			let accounts: Vec<_> = combined_iter.take((rebag_budget + 1) as usize).collect();

			// Safe split: if we reached (or passed) the tail of the list, we don't want to panic.
			let (to_process, next_cursor) = if accounts.len() <= rebag_budget as usize {
				// This guarantees we either get the next account to process
				// or gracefully receive None.
				(accounts.as_slice(), &[][..])
			} else {
				accounts.split_at(rebag_budget as usize)
			};

			let mut processed = 0u32;
			let mut successful_rebags = 0u32;
			let mut failed_rebags = 0u32;
			let mut pending_processed = 0u32;

			// First item's weight was already consumed above.
			for (i, account) in to_process.iter().enumerate() {
				// Consume weight for every item after the first.
				if i > 0 && meter.try_consume(per_item).is_err() {
					break;
				}

				let pending_value = if PendingRebag::<T, I>::contains_key(account) { 1 } else { 0 };

				match Self::rebag_internal(account) {
					Err(Error::<T, I>::Locked) => {
						defensive!("Pallet became locked during auto-rebag, stopping");
						break;
					},
					Err(e) => {
						log!(warn, "Error during rebagging: {:?}", e);
						failed_rebags += 1;
					},
					Ok(Some((from, to))) => {
						log!(debug, "Rebagged {:?}: moved from {:?} to {:?}", account, from, to);
						successful_rebags += 1;
						pending_processed += pending_value;
					},
					Ok(None) => {
						log!(debug, "Rebagging not needed for {:?}", account);
						pending_processed += pending_value;
					},
				}

				processed += 1;
				if processed == rebag_budget {
					break;
				}
			}

			// Update cursor - only track regular ListNodes accounts, not PendingRebag
			let next_regular_account =
				next_cursor.iter().find(|account| !PendingRebag::<T, I>::contains_key(account));

			match next_regular_account {
				// Defensive check: prevents re-processing the same node multiple times within a
				// single block. This situation should not occur during normal execution, but
				// can happen in test environments or if `on_idle()` is invoked more than once
				// per block (e.g. via custom test harnesses or manual calls).
				Some(next) if to_process.contains(next) => {
					NextNodeAutoRebagged::<T, I>::kill();
					defensive!("Loop detected: {:?} already processed — cursor killed", next);
				},
				// Normal case: save the next regular node as a cursor for the following block.
				Some(next) => {
					NextNodeAutoRebagged::<T, I>::put(next);
					log!(debug, "Saved next node to be processed in rebag cursor: {:?}", next);
				},
				// End of regular list reached: no cursor needed.
				// This happens when either:
				// 1. We've processed all regular accounts in the list, OR
				// 2. We've collected fewer than budget+1 accounts (meaning the iterator was
				//    exhausted)
				// Since pending accounts are processed first and not tracked in the cursor,
				// this simply means there are no more regular accounts to process.
				None => {
					NextNodeAutoRebagged::<T, I>::kill();
					log!(debug, "End of regular list reached — cursor killed");
				},
			}

			let weight_used = meter.consumed();
			log!(
				debug,
				"Auto-rebag finished: processed={}, successful_rebags={}, errors={}, pending_processed={}, weight_used={:?}",
				processed,
				successful_rebags,
				failed_rebags,
				pending_processed,
				weight_used
			);

			weight_used
		}
	}
}

#[cfg(any(test, feature = "try-runtime", feature = "fuzz"))]
impl<T: Config<I>, I: 'static> Pallet<T, I> {
	pub fn do_try_state() -> Result<(), TryRuntimeError> {
		List::<T, I>::do_try_state()
	}
}

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	/// Move an account from one bag to another, depositing an event on success.
	///
	/// If the account changed bags, returns `Ok(Some((from, to)))`.
	pub fn do_rebag(
		account: &T::AccountId,
		new_score: T::Score,
	) -> Result<Option<(T::Score, T::Score)>, ListError> {
		// If no voter at that node, don't do anything. the caller just wasted the fee to call this.
		let node = list::Node::<T, I>::get(&account).ok_or(ListError::NodeNotFound)?;
		if node.score != new_score {
			Self::deposit_event(Event::<T, I>::ScoreUpdated { who: account.clone(), new_score });
		}
		let maybe_movement = List::update_position_for(node, new_score);
		if let Some((from, to)) = maybe_movement {
			Self::deposit_event(Event::<T, I>::Rebagged { who: account.clone(), from, to });
		};
		Ok(maybe_movement)
	}

	fn ensure_unlocked() -> Result<(), ListError> {
		match Lock::<T, I>::get() {
			None => Ok(()),
			Some(()) => Err(ListError::Locked),
		}
	}

	/// Equivalent to `ListBags::get`, but public. Useful for tests in outside of this crate.
	#[cfg(feature = "std")]
	pub fn list_bags_get(score: T::Score) -> Option<list::Bag<T, I>> {
		ListBags::get(score)
	}

	/// Perform the internal rebagging logic for an account based on its updated score.
	/// This function does not handle origin checks or higher-level dispatch logic.
	///
	/// Returns `Ok(Some((from, to)))` if rebagging occurred, or `Ok(None)` if nothing changed.
	fn rebag_internal(account: &T::AccountId) -> Result<Option<(T::Score, T::Score)>, Error<T, I>> {
		// Ensure the pallet is not locked
		Self::ensure_unlocked().map_err(|_| Error::<T, I>::Locked)?;

		PendingRebag::<T, I>::remove(account);

		// Check if the account exists and retrieve its current score
		let existed = ListNodes::<T, I>::contains_key(account);
		let maybe_score = T::ScoreProvider::score(account);

		match (existed, maybe_score) {
			(true, Some(current_score)) => {
				// The account exists and has a valid score, so try to rebag
				log!(debug, "Attempting to rebag node {:?}", account);
				Pallet::<T, I>::do_rebag(account, current_score)
					.map_err::<Error<T, I>, _>(Into::into)
			},
			(false, Some(current_score)) => {
				// The account doesn't exist, but it has a valid score - insert it
				log!(debug, "Inserting node {:?} with score {:?}", account, current_score);
				List::<T, I>::insert(account.clone(), current_score)
					.map_err::<Error<T, I>, _>(Into::into)?;
				Ok(None)
			},
			(true, None) => {
				// The account exists but no longer has a valid score, so remove it
				log!(debug, "Removing node {:?}", account);
				List::<T, I>::remove(account).map_err::<Error<T, I>, _>(Into::into)?;
				Ok(None)
			},
			(false, None) => {
				// The account doesn't exist and has no valid score - do nothing
				Err(Error::<T, I>::List(ListError::NodeNotFound))
			},
		}
	}
}

impl<T: Config<I>, I: 'static> SortedListProvider<T::AccountId> for Pallet<T, I> {
	type Error = ListError;
	type Score = T::Score;

	fn range() -> (Self::Score, Self::Score) {
		use frame_support::traits::Get;
		(
			T::BagThresholds::get().first().cloned().unwrap_or_default(),
			T::BagThresholds::get().last().cloned().unwrap_or_default(),
		)
	}

	fn iter() -> Box<dyn Iterator<Item = T::AccountId>> {
		Box::new(List::<T, I>::iter().map(|n| n.id().clone()))
	}

	fn lock() {
		Lock::<T, I>::put(())
	}

	fn unlock() {
		Lock::<T, I>::kill()
	}

	fn iter_from(
		start: &T::AccountId,
	) -> Result<Box<dyn Iterator<Item = T::AccountId>>, Self::Error> {
		let iter = List::<T, I>::iter_from(start)?;
		Ok(Box::new(iter.map(|n| n.id().clone())))
	}

	fn count() -> u32 {
		ListNodes::<T, I>::count()
	}

	fn contains(id: &T::AccountId) -> bool {
		List::<T, I>::contains(id)
	}

	fn on_insert(id: T::AccountId, score: T::Score) -> Result<(), ListError> {
		if Pallet::<T, I>::ensure_unlocked().is_err() {
			// Pallet is locked - store in PendingRebag for later processing
			// Only queue if auto-rebagging is enabled
			if T::MaxAutoRebagPerBlock::get() > 0u32 {
				PendingRebag::<T, I>::insert(&id, ());
				return Ok(());
			}

			return Err(ListError::Locked);
		};
		List::<T, I>::insert(id, score)
	}

	fn on_update(id: &T::AccountId, new_score: T::Score) -> Result<(), ListError> {
		Pallet::<T, I>::ensure_unlocked()?;
		Pallet::<T, I>::do_rebag(id, new_score).map(|_| ())
	}

	fn get_score(id: &T::AccountId) -> Result<T::Score, ListError> {
		List::<T, I>::get_score(id)
	}

	fn on_remove(id: &T::AccountId) -> Result<(), ListError> {
		Pallet::<T, I>::ensure_unlocked()?;
		List::<T, I>::remove(id)
	}

	fn unsafe_regenerate(
		all: impl IntoIterator<Item = T::AccountId>,
		score_of: Box<dyn Fn(&T::AccountId) -> Option<T::Score>>,
	) -> u32 {
		// NOTE: This call is unsafe for the same reason as SortedListProvider::unsafe_regenerate.
		// I.e. because it can lead to many storage accesses.
		// So it is ok to call it as caller must ensure the conditions.
		List::<T, I>::unsafe_regenerate(all, score_of)
	}

	fn unsafe_clear() {
		// NOTE: This call is unsafe for the same reason as SortedListProvider::unsafe_clear.
		// I.e. because it can lead to many storage accesses.
		// So it is ok to call it as caller must ensure the conditions.
		List::<T, I>::unsafe_clear()
	}

	#[cfg(feature = "try-runtime")]
	fn try_state() -> Result<(), TryRuntimeError> {
		Self::do_try_state()
	}

	frame_election_provider_support::runtime_benchmarks_enabled! {
		fn score_update_worst_case(who: &T::AccountId, is_increase: bool) -> Self::Score {
			use frame_support::traits::Get as _;
			let thresholds = T::BagThresholds::get();
			let node = list::Node::<T, I>::get(who).unwrap();
			let current_bag_idx = thresholds
				.iter()
				.chain(core::iter::once(&T::Score::max_value()))
				.position(|w| w == &node.bag_upper)
				.unwrap();

			if is_increase {
				let next_threshold_idx = current_bag_idx + 1;
				assert!(thresholds.len() > next_threshold_idx);
				thresholds[next_threshold_idx]
			} else {
				assert!(current_bag_idx != 0);
				let prev_threshold_idx = current_bag_idx - 1;
				thresholds[prev_threshold_idx]
			}
		}
	}
}

impl<T: Config<I>, I: 'static> ScoreProvider<T::AccountId> for Pallet<T, I> {
	type Score = <Pallet<T, I> as SortedListProvider<T::AccountId>>::Score;

	fn score(id: &T::AccountId) -> Option<T::Score> {
		Node::<T, I>::get(id).map(|node| node.score())
	}

	frame_election_provider_support::runtime_benchmarks_or_std_enabled! {
		fn set_score_of(id: &T::AccountId, new_score: T::Score) {
			ListNodes::<T, I>::mutate(id, |maybe_node| {
				if let Some(node) = maybe_node.as_mut() {
					node.score = new_score;
				} else {
					panic!("trying to mutate {:?} which does not exists", id);
				}
			})
		}
	}
}
