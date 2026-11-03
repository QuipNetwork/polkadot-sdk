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

//! A pallet for any shared state that other pallets may want access to.
//!
//! To avoid cyclic dependencies, it is important that this pallet is not
//! dependent on any of the other pallets.

use alloc::{
	collections::{btree_map::BTreeMap, btree_set::BTreeSet, vec_deque::VecDeque},
	vec::Vec,
};
use frame_support::{pallet_prelude::*, traits::DisabledValidators};
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_primitives::{
	transpose_claim_queue, vstaging::RelayParentInfo, CoreIndex, Id as ParaId, Id, SessionIndex,
	ValidatorId, ValidatorIndex,
};
use sp_runtime::traits::AtLeast32BitUnsigned;

use rand::{seq::SliceRandom, SeedableRng};
use rand_chacha::ChaCha20Rng;

use crate::configuration::HostConfiguration;

pub use pallet::*;

// `SESSION_DELAY` is used to delay any changes to Paras registration or configurations.
// Wait until the session index is 2 larger then the current index to apply any changes,
// which guarantees that at least one full session has passed before any changes are applied.
pub(crate) const SESSION_DELAY: SessionIndex = 2;

#[cfg(test)]
mod tests;

pub mod migration;

/// Information about a scheduling parent.
#[derive(Encode, Decode, Default, TypeInfo, Debug)]
pub struct SchedulingParentInfo<Hash> {
	// Scheduling parent hash
	pub scheduling_parent: Hash,
	// Claim queue snapshot, optimized for accessing the assignments by `ParaId`.
	// For each para we store the cores assigned per depth.
	pub claim_queue: BTreeMap<Id, BTreeMap<u8, BTreeSet<CoreIndex>>>,
}

/// Keeps tracks of information about all viable scheduling parents.
#[derive(Encode, Decode, Default, TypeInfo)]
pub struct AllowedSchedulingParentsTracker<Hash, BlockNumber> {
	// Information about past scheduling parents that are viable to use for backing.
	//
	// They are in ascending chronologic order, so the newest scheduling parents are at
	// the back of the deque.
	buffer: VecDeque<SchedulingParentInfo<Hash>>,

	// The number of the most recent scheduling-parent, if any.
	// If the buffer is empty, this value has no meaning and may
	// be nonsensical.
	latest_number: BlockNumber,
}

impl<Hash: PartialEq + Copy, BlockNumber: AtLeast32BitUnsigned + Copy>
	AllowedSchedulingParentsTracker<Hash, BlockNumber>
{
	/// Add a new scheduling-parent to the allowed scheduling parents, along with info about the
	/// header. Provide a maximum ancestry length for the buffer, which will cause old
	/// scheduling-parents to be pruned.
	/// If the scheduling parent hash is already present, do nothing.
	pub(crate) fn update(
		&mut self,
		scheduling_parent: Hash,
		claim_queue: BTreeMap<CoreIndex, VecDeque<Id>>,
		number: BlockNumber,
		max_ancestry_len: u32,
	) {
		if self.buffer.iter().any(|info| info.scheduling_parent == scheduling_parent) {
			// Already present.
			return;
		}

		let claim_queue = transpose_claim_queue(claim_queue);

		self.buffer.push_back(SchedulingParentInfo { scheduling_parent, claim_queue });

		self.latest_number = number;
		while self.buffer.len() > (max_ancestry_len as usize) {
			let _ = self.buffer.pop_front();
		}

		// We only allow scheduling parents within the same sessions, the buffer
		// gets cleared on session changes.
	}

	/// Attempt to acquire the state root and block number to be used when building
	/// upon the given scheduling-parent.
	pub(crate) fn acquire_info(
		&self,
		scheduling_parent: Hash,
	) -> Option<(&SchedulingParentInfo<Hash>, BlockNumber)> {
		let pos = self
			.buffer
			.iter()
			.position(|info| info.scheduling_parent == scheduling_parent)?;
		let age = (self.buffer.len() - 1) - pos;
		let number = self.latest_number - BlockNumber::from(age as u32);

		Some((&self.buffer[pos], number))
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(2);

	#[pallet::pallet]
	#[pallet::without_storage_info]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type DisabledValidators: frame_support::traits::DisabledValidators;
	}

	/// The current session index.
	#[pallet::storage]
	pub type CurrentSessionIndex<T: Config> = StorageValue<_, SessionIndex, ValueQuery>;

	/// All the validators actively participating in parachain consensus.
	/// Indices are into the broader validator set.
	#[pallet::storage]
	pub type ActiveValidatorIndices<T: Config> = StorageValue<_, Vec<ValidatorIndex>, ValueQuery>;

	/// The parachain attestation keys of the validators actively participating in parachain
	/// consensus. This should be the same length as `ActiveValidatorIndices`.
	#[pallet::storage]
	pub type ActiveValidatorKeys<T: Config> = StorageValue<_, Vec<ValidatorId>, ValueQuery>;

	/// All allowed scheduling parents.
	#[pallet::storage]
	pub(crate) type AllowedSchedulingParents<T: Config> =
		StorageValue<_, AllowedSchedulingParentsTracker<T::Hash, BlockNumberFor<T>>, ValueQuery>;

	/// All allowed relay parents, keyed by (session_index, relay_parent_hash).
	#[pallet::storage]
	pub(crate) type AllowedRelayParents<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		SessionIndex,
		Blake2_128Concat,
		T::Hash,
		RelayParentInfo<T::Hash, BlockNumberFor<T>>,
	>;

	/// The oldest session index for which we still have relay parent entries in
	/// `AllowedRelayParents`. Used to efficiently prune all expired sessions
	/// when `max_relay_parent_session_age` decreases.
	#[pallet::storage]
	pub(crate) type OldestRelayParentSession<T: Config> = StorageValue<_, SessionIndex, ValueQuery>;

	/// The minimum relay parent block number for each session that has entries in
	/// `AllowedRelayParents`. This is the block number of the first relay parent
	/// added to each session.
	#[pallet::storage]
	pub(crate) type MinimumRelayParentNumber<T: Config> =
		StorageMap<_, Twox64Concat, SessionIndex, BlockNumberFor<T>>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

impl<T: Config> Pallet<T> {
	/// Called by the initializer to initialize the configuration pallet.
	pub(crate) fn initializer_initialize(_now: BlockNumberFor<T>) -> Weight {
		Weight::zero()
	}

	/// Called by the initializer to finalize the configuration pallet.
	pub(crate) fn initializer_finalize() {}

	/// Called by the initializer to note that a new session has started.
	///
	/// Returns the list of outgoing paras from the actions queue.
	pub(crate) fn initializer_on_new_session(
		session_index: SessionIndex,
		random_seed: [u8; 32],
		new_config: &HostConfiguration<BlockNumberFor<T>>,
		all_validators: Vec<ValidatorId>,
	) -> Vec<ValidatorId> {
		// Drop allowed scheduling parents buffer on a session change.
		//
		// During the initialization of the next block we always add its parent
		// to the tracker.
		//
		// With asynchronous backing candidates built on top of scheduling
		// parent `R` are still restricted by the runtime to be backed
		// by the group assigned at `number(R) + 1`, which is guaranteed
		// to be in the current session.
		AllowedSchedulingParents::<T>::mutate(|tracker| tracker.buffer.clear());

		// Prune relay parents from sessions that are now too old.
		let oldest_allowed_session =
			session_index.saturating_sub(new_config.max_relay_parent_session_age);
		let oldest_stored = OldestRelayParentSession::<T>::get();

		// Only prune and advance the pointer if the allowed oldest session has moved
		// forward. If max_relay_parent_session_age was increased at runtime,
		// oldest_allowed_session may be less than oldest_stored; in that case, entries
		// for those older sessions were already pruned in prior blocks and we must not
		// move the pointer backward.
		if oldest_allowed_session > oldest_stored {
			for expired in oldest_stored..oldest_allowed_session {
				let _ = AllowedRelayParents::<T>::clear_prefix(expired, u32::MAX, None);
				MinimumRelayParentNumber::<T>::remove(expired);
			}
			OldestRelayParentSession::<T>::set(oldest_allowed_session);
		}

		CurrentSessionIndex::<T>::set(session_index);
		let mut rng: ChaCha20Rng = SeedableRng::from_seed(random_seed);

		let mut shuffled_indices: Vec<_> = (0..all_validators.len())
			.enumerate()
			.map(|(i, _)| ValidatorIndex(i as _))
			.collect();

		shuffled_indices.shuffle(&mut rng);

		if let Some(max) = new_config.max_validators {
			shuffled_indices.truncate(max as usize);
		}

		let active_validator_keys =
			crate::util::take_active_subset(&shuffled_indices, &all_validators);

		ActiveValidatorIndices::<T>::set(shuffled_indices);
		ActiveValidatorKeys::<T>::set(active_validator_keys.clone());

		active_validator_keys
	}

	/// Return the session index that should be used for any future scheduled changes.
	pub fn scheduled_session() -> SessionIndex {
		CurrentSessionIndex::<T>::get().saturating_add(SESSION_DELAY)
	}

	/// Fetches disabled validators list from session pallet.
	/// CAVEAT: this might produce incorrect results on session boundaries
	pub fn disabled_validators() -> Vec<ValidatorIndex> {
		let shuffled_indices = ActiveValidatorIndices::<T>::get();
		// mapping from raw validator index to `ValidatorIndex`
		// this computation is the same within a session, but should be cheap
		let reverse_index = shuffled_indices
			.iter()
			.enumerate()
			.map(|(i, v)| (v.0, ValidatorIndex(i as u32)))
			.collect::<BTreeMap<u32, ValidatorIndex>>();

		// we might have disabled validators who are not parachain validators
		T::DisabledValidators::disabled_validators()
			.iter()
			.filter_map(|v| reverse_index.get(v).cloned())
			.collect()
	}

	/// Called at the beginning of each block to update the allowed scheduling and relay parents.
	///
	/// Adds the parent block as an allowed scheduling parent and relay parent.
	pub fn new_block(
		hash: T::Hash,
		cq: BTreeMap<CoreIndex, VecDeque<ParaId>>,
		block_number: BlockNumberFor<T>,
		max_ancestry_len: u32,
		storage_root: T::Hash,
		session_index: SessionIndex,
	) {
		// Update the allowed scheduling parents.
		AllowedSchedulingParents::<T>::mutate(|tracker| {
			tracker.update(hash, cq, block_number, max_ancestry_len);
		});

		// Insert this block's parent as an allowed relay parent for the current session.
		AllowedRelayParents::<T>::insert(
			session_index,
			hash,
			RelayParentInfo { number: block_number, state_root: storage_root },
		);

		// Track the minimum relay parent number for this session.
		// Only set on the first relay parent of the session (subsequent blocks have
		// higher numbers).
		if !MinimumRelayParentNumber::<T>::contains_key(session_index) {
			MinimumRelayParentNumber::<T>::insert(session_index, block_number);
		}
	}

	/// Retrieve relay parent info by session index and relay parent hash.
	pub fn get_relay_parent_info(
		session_index: SessionIndex,
		relay_parent: T::Hash,
	) -> Option<RelayParentInfo<T::Hash, BlockNumberFor<T>>> {
		AllowedRelayParents::<T>::get(session_index, relay_parent)
	}

	/// Get the minimum allowed relay parent block number across all sessions.
	/// Returns the minimum from the oldest session's entry in `MinimumRelayParentNumber`.
	pub fn get_minimum_relay_parent_number() -> Option<BlockNumberFor<T>> {
		let oldest_session = OldestRelayParentSession::<T>::get();
		MinimumRelayParentNumber::<T>::get(oldest_session)
	}

	/// Test function for setting the current session index.
	#[cfg(any(feature = "std", feature = "runtime-benchmarks", test))]
	pub fn set_session_index(index: SessionIndex) {
		CurrentSessionIndex::<T>::set(index);
	}

	#[cfg(any(feature = "std", feature = "runtime-benchmarks", test))]
	pub fn set_active_validators_ascending(active: Vec<ValidatorId>) {
		ActiveValidatorIndices::<T>::set(
			(0..active.len()).map(|i| ValidatorIndex(i as _)).collect(),
		);
		ActiveValidatorKeys::<T>::set(active);
	}

	#[cfg(test)]
	pub(crate) fn set_active_validators_with_indices(
		indices: Vec<ValidatorIndex>,
		keys: Vec<ValidatorId>,
	) {
		assert_eq!(indices.len(), keys.len());
		ActiveValidatorIndices::<T>::set(indices);
		ActiveValidatorKeys::<T>::set(keys);
	}

	#[cfg(test)]
	pub(crate) fn add_allowed_scheduling_parent(
		scheduling_parent: T::Hash,
		claim_queue: BTreeMap<CoreIndex, VecDeque<Id>>,
		number: BlockNumberFor<T>,
		max_ancestry_len: u32,
	) {
		AllowedSchedulingParents::<T>::mutate(|tracker| {
			tracker.update(scheduling_parent, claim_queue, number, max_ancestry_len + 1)
		});

		// Also populate the AllowedRelayParents DoubleMap so that tests
		// which call verify_backed_candidate can look up relay parent info.
		let session_index = CurrentSessionIndex::<T>::get();
		AllowedRelayParents::<T>::insert(
			session_index,
			scheduling_parent,
			RelayParentInfo { number, state_root: Default::default() },
		);
	}
}
