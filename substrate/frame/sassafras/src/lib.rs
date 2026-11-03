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

//! Extension module for Sassafras consensus.
//!
//! [Sassafras](https://research.web3.foundation/Polkadot/protocols/block-production/SASSAFRAS)
//! is a constant-time block production protocol that aims to ensure that there is
//! exactly one block produced with constant time intervals rather than multiple or none.
//!
//! We run a lottery to distribute block production slots in an epoch and to fix the
//! order validators produce blocks in, by the beginning of an epoch.
//!
//! Each validator signs the same VRF input and publishes the output on-chain. This
//! value is their lottery ticket that can be validated against their public key.
//!
//! We want to keep lottery winners secret, i.e. do not publish their public keys.
//! At the beginning of the epoch all the validators tickets are published but not
//! their public keys.
//!
//! A valid tickets is validated when an honest validator reclaims it on block
//! production.
//!
//! To prevent submission of fake tickets, resulting in empty slots, the validator
//! when submitting the ticket accompanies it with a SNARK of the statement: "Here's
//! my VRF output that has been generated using the given VRF input and my secret
//! key. I'm not telling you my keys, but my public key is among those of the
//! nominated validators", that is validated before the lottery.
//!
//! To anonymously publish the ticket to the chain a validator sends their tickets
//! to a random validator who later puts it on-chain as a transaction.

#![deny(warnings)]
#![warn(unused_must_use, unsafe_code, unused_variables, unused_imports, missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{Decode, Encode, MaxEncodedLen};
use log::{debug, error, trace, warn};
use scale_info::TypeInfo;

use alloc::vec::Vec;
use frame_support::{
	dispatch::{DispatchResultWithPostInfo, Pays},
	traits::{Defensive, Get},
	weights::Weight,
	BoundedVec, WeakBoundedVec,
};
use frame_system::{
	offchain::{CreateBare, SubmitTransaction},
	pallet_prelude::BlockNumberFor,
};
use sp_consensus_sassafras::{
	digests::{ConsensusLog, NextEpochDescriptor, SlotClaim},
	vrf, AuthorityId, Epoch, EpochConfiguration, Randomness, Slot, TicketBody, TicketEnvelope,
	TicketId, RANDOMNESS_LENGTH, SASSAFRAS_ENGINE_ID,
};
use sp_io::hashing;
use sp_runtime::{
	generic::DigestItem,
	traits::{One, Zero},
	BoundToRuntimeAppPublic,
};

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(all(feature = "std", test))]
mod mock;
#[cfg(all(feature = "std", test))]
mod tests;

pub mod weights;
pub use weights::WeightInfo;

pub use pallet::*;

const LOG_TARGET: &str = "sassafras::runtime";

// Max length for segments holding unsorted tickets.
const SEGMENT_MAX_SIZE: u32 = 128;

/// Authorities bounded vector convenience type.
pub type AuthoritiesVec<T> = WeakBoundedVec<AuthorityId, <T as Config>::MaxAuthorities>;

/// Epoch length defined by the configuration.
pub type EpochLengthFor<T> = <T as Config>::EpochLength;

/// Tickets metadata.
#[derive(Debug, Default, PartialEq, Encode, Decode, TypeInfo, MaxEncodedLen, Clone, Copy)]
pub struct TicketsMetadata {
	/// Number of outstanding next epoch tickets requiring to be sorted.
	///
	/// These tickets are held by the [`UnsortedSegments`] storage map in segments
	/// containing at most `SEGMENT_MAX_SIZE` items.
	pub unsorted_tickets_count: u32,

	/// Number of tickets available for current and next epoch.
	///
	/// These tickets are held by the [`TicketsIds`] storage map.
	///
	/// The array entry to be used for the current epoch is computed as epoch index modulo 2.
	pub tickets_count: [u32; 2],
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	/// The Sassafras pallet.
	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Configuration parameters.
	#[pallet::config]
	pub trait Config: frame_system::Config + CreateBare<Call<Self>> {
		/// Amount of slots that each epoch should last.
		#[pallet::constant]
		type EpochLength: Get<u32>;

		/// Max number of authorities allowed.
		#[pallet::constant]
		type MaxAuthorities: Get<u32>;

		/// Epoch change trigger.
		///
		/// Logic to be triggered on every block to query for whether an epoch has ended
		/// and to perform the transition to the next epoch.
		type EpochChangeTrigger: EpochChangeTrigger;

		/// Weight information for all calls of this pallet.
		type WeightInfo: WeightInfo;
	}

	/// Sassafras runtime errors.
	#[pallet::error]
	pub enum Error<T> {
		/// Submitted configuration is invalid.
		InvalidConfiguration,
	}

	/// Current epoch index.
	#[pallet::storage]
	pub type EpochIndex<T> = StorageValue<_, u64, ValueQuery>;

	/// Current epoch authorities.
	#[pallet::storage]
	pub type Authorities<T: Config> = StorageValue<_, AuthoritiesVec<T>, ValueQuery>;

	/// Next epoch authorities.
	#[pallet::storage]
	pub type NextAuthorities<T: Config> = StorageValue<_, AuthoritiesVec<T>, ValueQuery>;

	/// First block slot number.
	///
	/// As the slots may not be zero-based, we record the slot value for the fist block.
	/// This allows to always compute relative indices for epochs and slots.
	#[pallet::storage]
	pub type GenesisSlot<T> = StorageValue<_, Slot, ValueQuery>;

	/// Current block slot number.
	#[pallet::storage]
	pub type CurrentSlot<T> = StorageValue<_, Slot, ValueQuery>;

	/// Current epoch randomness.
	#[pallet::storage]
	pub type CurrentRandomness<T> = StorageValue<_, Randomness, ValueQuery>;

	/// Next epoch randomness.
	#[pallet::storage]
	pub type NextRandomness<T> = StorageValue<_, Randomness, ValueQuery>;

	/// Randomness accumulator.
	///
	/// Excluded the first imported block, its value is updated on block finalization.
	#[pallet::storage]
	pub(crate) type RandomnessAccumulator<T> = StorageValue<_, Randomness, ValueQuery>;

	/// Per slot randomness used to feed the randomness accumulator.
	///
	/// The value is ephemeral and is cleared on block finalization.
	#[pallet::storage]
	pub(crate) type SlotRandomness<T> = StorageValue<_, Randomness>;

	/// The configuration for the current epoch.
	#[pallet::storage]
	pub type EpochConfig<T> = StorageValue<_, EpochConfiguration, ValueQuery>;

	/// The configuration for the next epoch.
	#[pallet::storage]
	pub type NextEpochConfig<T> = StorageValue<_, EpochConfiguration>;

	/// Pending epoch configuration change that will be set as `NextEpochConfig` when the next
	/// epoch is enacted.
	///
	/// In other words, a configuration change submitted during epoch N will be enacted on epoch
	/// N+2. This is to maintain coherence for already submitted tickets for epoch N+1 that where
	/// computed using configuration parameters stored for epoch N+1.
	#[pallet::storage]
	pub type PendingEpochConfigChange<T> = StorageValue<_, EpochConfiguration>;

	/// Stored tickets metadata.
	#[pallet::storage]
	pub type TicketsMeta<T> = StorageValue<_, TicketsMetadata, ValueQuery>;

	/// Tickets identifiers map.
	///
	/// The map holds tickets ids for the current and next epoch.
	///
	/// The key is a tuple composed by:
	/// - `u8` equal to epoch's index modulo 2;
	/// - `u32` equal to the ticket's index in a sorted list of epoch's tickets.
	///
	/// Epoch X first N-th ticket has key (X mod 2, N)
	///
	/// Note that the ticket's index doesn't directly correspond to the slot index within the epoch.
	/// The assignment is computed dynamically using an *outside-in* strategy.
	///
	/// Be aware that entries within this map are never removed, only overwritten.
	/// Last element index should be fetched from the [`TicketsMeta`] value.
	#[pallet::storage]
	pub type TicketsIds<T> = StorageMap<_, Identity, (u8, u32), TicketId>;

	/// Tickets to be used for current and next epoch.
	#[pallet::storage]
	pub type TicketsData<T> = StorageMap<_, Identity, TicketId, TicketBody>;

	/// Next epoch tickets unsorted segments.
	///
	/// Contains lists of tickets where each list represents a batch of tickets
	/// received via the `submit_tickets` extrinsic.
	///
	/// Each segment has max length [`SEGMENT_MAX_SIZE`].
	#[pallet::storage]
	pub type UnsortedSegments<T: Config> =
		StorageMap<_, Identity, u32, BoundedVec<TicketId, ConstU32<SEGMENT_MAX_SIZE>>, ValueQuery>;

	/// The most recently set of tickets which are candidates to become the next
	/// epoch tickets.
	#[pallet::storage]
	pub type SortedCandidates<T> =
		StorageValue<_, BoundedVec<TicketId, EpochLengthFor<T>>, ValueQuery>;

	/// Parameters used to construct the epoch's ring verifier.
	///
	/// In practice: Updatable Universal Reference String and the seed.
	#[pallet::storage]
	pub type RingContext<T: Config> = StorageValue<_, vrf::RingContext>;

	/// Ring verifier data for the current epoch.
	#[pallet::storage]
	pub type RingVerifierData<T: Config> = StorageValue<_, vrf::RingVerifierKey>;

	/// Genesis configuration for Sassafras protocol.
	#[pallet::genesis_config]
	#[derive(frame_support::DefaultNoBound)]
	pub struct GenesisConfig<T: Config> {
		/// Genesis authorities.
		pub authorities: Vec<AuthorityId>,
		/// Genesis epoch configuration.
		pub epoch_config: EpochConfiguration,
		/// Phantom config
		#[serde(skip)]
		pub _phantom: core::marker::PhantomData<T>,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			EpochConfig::<T>::put(self.epoch_config);
			Pallet::<T>::genesis_authorities_initialize(&self.authorities);

			#[cfg(feature = "construct-dummy-ring-context")]
			{
				debug!(target: LOG_TARGET, "Constructing dummy ring context");
				let ring_ctx = vrf::RingContext::new_testing();
				RingContext::<T>::put(ring_ctx);
				Pallet::<T>::update_ring_verifier(&self.authorities);
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(block_num: BlockNumberFor<T>) -> Weight {
			debug_assert_eq!(block_num, frame_system::Pallet::<T>::block_number());

			let claim = <frame_system::Pallet<T>>::digest()
				.logs
				.iter()
				.find_map(|item| item.pre_runtime_try_to::<SlotClaim>(&SASSAFRAS_ENGINE_ID))
				.expect("Valid block must have a slot claim. qed");

			CurrentSlot::<T>::put(claim.slot);

			if block_num == One::one() {
				Self::post_genesis_initialize(claim.slot);
			}

			let randomness = claim.vrf_signature.pre_output.make_bytes();
			SlotRandomness::<T>::put(randomness);

			let trigger_weight = T::EpochChangeTrigger::trigger::<T>(block_num);

			T::WeightInfo::on_initialize() + trigger_weight
		}

		fn on_finalize(_: BlockNumberFor<T>) {
			// At the end of the block, we can safely include the current slot randomness
			// to the accumulator. If we've determined that this block was the first in
			// a new epoch, the changeover logic has already occurred at this point
			// (i.e. `enact_epoch_change` has already been called).
			let randomness = SlotRandomness::<T>::take()
				.expect("Unconditionally populated in `on_initialize`; `on_finalize` is always called after; qed");
			Self::deposit_slot_randomness(&randomness);

			// Check if we are in the epoch's second half.
			// If so, start sorting the next epoch tickets.
			let epoch_length = T::EpochLength::get();
			let current_slot_idx = Self::current_slot_index();
			if current_slot_idx >= epoch_length / 2 {
				let mut metadata = TicketsMeta::<T>::get();
				if metadata.unsorted_tickets_count != 0 {
					let next_epoch_idx = EpochIndex::<T>::get() + 1;
					let next_epoch_tag = (next_epoch_idx & 1) as u8;
					let slots_left = epoch_length.checked_sub(current_slot_idx).unwrap_or(1);
					Self::sort_segments(
						metadata
							.unsorted_tickets_count
							.div_ceil(SEGMENT_MAX_SIZE * slots_left as u32),
						next_epoch_tag,
						&mut metadata,
					);
					TicketsMeta::<T>::set(metadata);
				}
			}
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Submit next epoch tickets candidates.
		///
		/// The number of tickets allowed to be submitted in one call is equal to the epoch length.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::submit_tickets(tickets.len() as u32))]
		pub fn submit_tickets(
			origin: OriginFor<T>,
			tickets: BoundedVec<TicketEnvelope, EpochLengthFor<T>>,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;

			debug!(target: LOG_TARGET, "Received {} tickets", tickets.len());

			let epoch_length = T::EpochLength::get();
			let current_slot_idx = Self::current_slot_index();
			if current_slot_idx > epoch_length / 2 {
				warn!(target: LOG_TARGET, "Tickets shall be submitted in the first epoch half",);
				return Err("Tickets shall be submitted in the first epoch half".into());
			}

			let Some(verifier) =
				RingVerifierData::<T>::get().map(|vk| vrf::RingContext::verifier_no_context(vk))
			else {
				warn!(target: LOG_TARGET, "Ring verifier key not initialized");
				return Err("Ring verifier key not initialized".into());
			};

			let next_authorities = NextAuthorities::<T>::get();

			// Compute tickets threshold
			let next_config =
				NextEpochConfig::<T>::get().unwrap_or_else(|| EpochConfig::<T>::get());
			let ticket_threshold = sp_consensus_sassafras::ticket_id_threshold(
				next_config.redundancy_factor,
				epoch_length as u32,
				next_config.attempts_number,
				next_authorities.len() as u32,
			);

			// Get next epoch params
			let randomness = NextRandomness::<T>::get();
			let epoch_idx = EpochIndex::<T>::get() + 1;

			let mut valid_tickets = BoundedVec::with_bounded_capacity(tickets.len());

			for ticket in tickets {
				debug!(target: LOG_TARGET, "Checking ring proof");

				// Check threshold constraint
				let ticket_id = vrf::make_ticket_id(&ticket.signature.pre_output);
				if ticket_id >= ticket_threshold {
					debug!(target: LOG_TARGET, "Ignoring ticket over threshold ({:032x} >= {:032x})", ticket_id, ticket_threshold);
					continue;
				}

				// Check for duplicates
				if TicketsData::<T>::contains_key(ticket_id) {
					debug!(target: LOG_TARGET, "Ignoring duplicate ticket ({:032x})", ticket_id);
					continue;
				}

				// Check ring signature
				let ticket_id_input =
					vrf::ticket_id_input(&randomness, ticket.body.attempt_idx, epoch_idx);
				let sign_data = vrf::ticket_body_sign_data(&ticket.body, ticket_id_input);
				if !ticket.signature.ring_vrf_verify(&sign_data, &verifier) {
					debug!(target: LOG_TARGET, "Proof verification failure for ticket ({:032x})", ticket_id);
					continue;
				}

				if let Ok(_) = valid_tickets.try_push(ticket_id).defensive_proof(
					"Input segment has same length as bounded destination vector; qed",
				) {
					TicketsData::<T>::set(ticket_id, Some(ticket.body));
				}
			}

			if !valid_tickets.is_empty() {
				Self::append_tickets(valid_tickets);
			}

			Ok(Pays::No.into())
		}

		/// Plan an epoch configuration change.
		///
		/// The epoch configuration change is recorded and will be announced at the beginning
		/// of the next epoch together with next epoch authorities information.
		/// In other words, the configuration will be enacted one epoch later.
		///
		/// Multiple calls to this method will replace any existing planned config change
		/// that has not been enacted yet.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::plan_config_change())]
		pub fn plan_config_change(
			origin: OriginFor<T>,
			config: EpochConfiguration,
		) -> DispatchResult {
			ensure_root(origin)?;

			ensure!(
				config.redundancy_factor != 0 && config.attempts_number != 0,
				Error::<T>::InvalidConfiguration
			);
			PendingEpochConfigChange::<T>::put(config);
			Ok(())
		}
	}

	#[allow(deprecated)]
	#[pallet::validate_unsigned]
	impl<T: Config> ValidateUnsigned for Pallet<T> {
		type Call = Call<T>;

		fn validate_unsigned(source: TransactionSource, call: &Self::Call) -> TransactionValidity {
			let Call::submit_tickets { tickets } = call else {
				return InvalidTransaction::Call.into();
			};

			// Discard tickets not coming from the local node or that are not included in a block
			if source == TransactionSource::External {
				warn!(
					target: LOG_TARGET,
					"Rejecting unsigned `submit_tickets` transaction from external source",
				);
				return InvalidTransaction::BadSigner.into();
			}

			// Current slot should be less than half of epoch length.
			let epoch_length = T::EpochLength::get();
			let current_slot_idx = Self::current_slot_index();
			if current_slot_idx > epoch_length / 2 {
				warn!(target: LOG_TARGET, "Tickets shall be proposed in the first epoch half",);
				return InvalidTransaction::Stale.into();
			}

			// This should be set such that it is discarded after the first epoch half
			let tickets_longevity = epoch_length / 2 - current_slot_idx;
			let tickets_tag = tickets.using_encoded(|bytes| hashing::blake2_256(bytes));

			ValidTransaction::with_tag_prefix("Sassafras")
				.priority(TransactionPriority::max_value())
				.longevity(tickets_longevity as u64)
				.and_provides(tickets_tag)
				.propagate(true)
				.build()
		}
	}
}

// Inherent methods
impl<T: Config> Pallet<T> {
	/// Determine whether an epoch change should take place at this block.
	///
	/// Assumes that initialization has already taken place.
	pub(crate) fn should_end_epoch(block_num: BlockNumberFor<T>) -> bool {
		// The epoch has technically ended during the passage of time between this block and the
		// last, but we have to "end" the epoch now, since there is no earlier possible block we
		// could have done it.
		//
		// The exception is for block 1: the genesis has slot 0, so we treat epoch 0 as having
		// started at the slot of block 1. We want to use the same randomness and validator set as
		// signalled in the genesis, so we don't rotate the epoch.
		block_num > One::one() && Self::current_slot_index() >= T::EpochLength::get()
	}

	/// Current slot index relative to the current epoch.
	fn current_slot_index() -> u32 {
		Self::slot_index(CurrentSlot::<T>::get())
	}

	/// Slot index relative to the current epoch.
	fn slot_index(slot: Slot) -> u32 {
		slot.checked_sub(*Self::current_epoch_start())
			.and_then(|v| v.try_into().ok())
			.unwrap_or(u32::MAX)
	}

	/// Finds the start slot of the current epoch.
	///
	/// Only guaranteed to give correct results after `initialize` of the first
	/// block in the chain (as its result is based off of `GenesisSlot`).
	fn current_epoch_start() -> Slot {
		Self::epoch_start(EpochIndex::<T>::get())
	}

	/// Get the epoch's first slot.
	fn epoch_start(epoch_index: u64) -> Slot {
		const PROOF: &str = "slot number is u64; it should relate in some way to wall clock time; \
							 if u64 is not enough we should crash for safety; qed.";

		let epoch_start = epoch_index.checked_mul(T::EpochLength::get() as u64).expect(PROOF);
		GenesisSlot::<T>::get().checked_add(epoch_start).expect(PROOF).into()
	}

	pub(crate) fn update_ring_verifier(authorities: &[AuthorityId]) {
		debug!(target: LOG_TARGET, "Loading ring context");
		let Some(ring_ctx) = RingContext::<T>::get() else {
			debug!(target: LOG_TARGET, "Ring context not initialized");
			return;
		};

		let pks: Vec<_> = authorities.iter().map(|auth| *auth.as_ref()).collect();

		debug!(target: LOG_TARGET, "Building ring verifier (ring size: {})", pks.len());
		let verifier_data = ring_ctx.verifier_key(&pks);

		RingVerifierData::<T>::put(verifier_data);
	}

	/// Enact an epoch change.
	///
	/// WARNING: Should be called on every block once and if and only if [`should_end_epoch`]
	/// has returned `true`.
	///
	/// If we detect one or more skipped epochs the policy is to use the authorities and values
	/// from the first skipped epoch. The tickets data is invalidated.
	pub(crate) fn enact_epoch_change(
		authorities: WeakBoundedVec<AuthorityId, T::MaxAuthorities>,
		next_authorities: WeakBoundedVec<AuthorityId, T::MaxAuthorities>,
	) {
		if next_authorities != authorities {
			Self::update_ring_verifier(&next_authorities);
		}

		// Update authorities
		Authorities::<T>::put(&authorities);
		NextAuthorities::<T>::put(&next_authorities);

		// Update epoch index
		let mut epoch_idx = EpochIndex::<T>::get() + 1;

		let slot_idx = CurrentSlot::<T>::get().saturating_sub(Self::epoch_start(epoch_idx));
		if slot_idx >= T::EpochLength::get() {
			// Detected one or more skipped epochs, clear tickets data and recompute epoch index.
			Self::reset_tickets_data();
			let skipped_epochs = *slot_idx / T::EpochLength::get() as u64;
			epoch_idx += skipped_epochs;
			warn!(
				target: LOG_TARGET,
				"Detected {} skipped epochs, resuming from epoch {}",
				skipped_epochs,
				epoch_idx
			);
		}

		let mut metadata = TicketsMeta::<T>::get();
		let mut metadata_dirty = false;

		EpochIndex::<T>::put(epoch_idx);

		let next_epoch_idx = epoch_idx + 1;

		// Updates current epoch randomness and computes the *next* epoch randomness.
		let next_randomness = Self::update_epoch_randomness(next_epoch_idx);

		if let Some(config) = NextEpochConfig::<T>::take() {
			EpochConfig::<T>::put(config);
		}

		let next_config = PendingEpochConfigChange::<T>::take();
		if let Some(next_config) = next_config {
			NextEpochConfig::<T>::put(next_config);
		}

		// After we update the current epoch, we signal the *next* epoch change
		// so that nodes can track changes.
		let next_epoch = NextEpochDescriptor {
			randomness: next_randomness,
			authorities: next_authorities.into_inner(),
			config: next_config,
		};
		Self::deposit_next_epoch_descriptor_digest(next_epoch);

		let epoch_tag = (epoch_idx & 1) as u8;

		// Optionally finish sorting
		if metadata.unsorted_tickets_count != 0 {
			Self::sort_segments(u32::MAX, epoch_tag, &mut metadata);
			metadata_dirty = true;
		}

		// Clear the "prev ≡ next (mod 2)" epoch tickets counter and bodies.
		// Ids are left since are just cyclically overwritten on-the-go.
		let prev_epoch_tag = epoch_tag ^ 1;
		let prev_epoch_tickets_count = &mut metadata.tickets_count[prev_epoch_tag as usize];
		if *prev_epoch_tickets_count != 0 {
			for idx in 0..*prev_epoch_tickets_count {
				if let Some(ticket_id) = TicketsIds::<T>::get((prev_epoch_tag, idx)) {
					TicketsData::<T>::remove(ticket_id);
				}
			}
			*prev_epoch_tickets_count = 0;
			metadata_dirty = true;
		}

		if metadata_dirty {
			TicketsMeta::<T>::set(metadata);
		}
	}

	// Call this function on epoch change to enact current epoch randomness.
	//
	// Returns the next epoch randomness.
	fn update_epoch_randomness(next_epoch_index: u64) -> Randomness {
		let curr_epoch_randomness = NextRandomness::<T>::get();
		CurrentRandomness::<T>::put(curr_epoch_randomness);

		let accumulator = RandomnessAccumulator::<T>::get();

		let mut buf = [0; RANDOMNESS_LENGTH + 8];
		buf[..RANDOMNESS_LENGTH].copy_from_slice(&accumulator[..]);
		buf[RANDOMNESS_LENGTH..].copy_from_slice(&next_epoch_index.to_le_bytes());

		let next_randomness = hashing::blake2_256(&buf);
		NextRandomness::<T>::put(&next_randomness);

		next_randomness
	}

	// Deposit per-slot randomness.
	fn deposit_slot_randomness(randomness: &Randomness) {
		let accumulator = RandomnessAccumulator::<T>::get();

		let mut buf = [0; 2 * RANDOMNESS_LENGTH];
		buf[..RANDOMNESS_LENGTH].copy_from_slice(&accumulator[..]);
		buf[RANDOMNESS_LENGTH..].copy_from_slice(&randomness[..]);

		let accumulator = hashing::blake2_256(&buf);
		RandomnessAccumulator::<T>::put(accumulator);
	}

	// Deposit next epoch descriptor in the block header digest.
	fn deposit_next_epoch_descriptor_digest(desc: NextEpochDescriptor) {
		let item = ConsensusLog::NextEpochData(desc);
		let log = DigestItem::Consensus(SASSAFRAS_ENGINE_ID, item.encode());
		<frame_system::Pallet<T>>::deposit_log(log)
	}

	// Initialize authorities on genesis phase.
	//
	// Genesis authorities may have been initialized via other means (e.g. via session pallet).
	//
	// If this function has already been called with some authorities, then the new list
	// should match the previously set one.
	fn genesis_authorities_initialize(authorities: &[AuthorityId]) {
		let prev_authorities = Authorities::<T>::get();

		if !prev_authorities.is_empty() {
			// This function has already been called.
			if prev_authorities.as_slice() == authorities {
				return;
			} else {
				panic!("Authorities were already initialized");
			}
		}

		let authorities = WeakBoundedVec::try_from(authorities.to_vec())
			.expect("Initial number of authorities should be lower than T::MaxAuthorities");
		Authorities::<T>::put(&authorities);
		NextAuthorities::<T>::put(&authorities);
	}

	// Method to be called on first block `on_initialize` to properly populate some key parameters.
	fn post_genesis_initialize(slot: Slot) {
		// Keep track of the actual first slot used (may not be zero based).
		GenesisSlot::<T>::put(slot);

		// Properly initialize randomness using genesis hash and current slot.
		// This is important to guarantee that a different set of tickets are produced for:
		// - different chains which share the same ring parameters and
		// - same chain started with a different slot base.
		let genesis_hash = frame_system::Pallet::<T>::parent_hash();
		let mut buf = genesis_hash.as_ref().to_vec();
		buf.extend_from_slice(&slot.to_le_bytes());
		let randomness = hashing::blake2_256(buf.as_slice());
		RandomnessAccumulator::<T>::put(randomness);

		let next_randomness = Self::update_epoch_randomness(1);

		// Deposit a log as this is the first block in first epoch.
		let next_epoch = NextEpochDescriptor {
			randomness: next_randomness,
			authorities: NextAuthorities::<T>::get().into_inner(),
			config: None,
		};
		Self::deposit_next_epoch_descriptor_digest(next_epoch);
	}

	/// Current epoch information.
	pub fn current_epoch() -> Epoch {
		let index = EpochIndex::<T>::get();
		Epoch {
			index,
			start: Self::epoch_start(index),
			length: T::EpochLength::get(),
			authorities: Authorities::<T>::get().into_inner(),
			randomness: CurrentRandomness::<T>::get(),
			config: EpochConfig::<T>::get(),
		}
	}

	/// Next epoch information.
	pub fn next_epoch() -> Epoch {
		let index = EpochIndex::<T>::get() + 1;
		Epoch {
			index,
			start: Self::epoch_start(index),
			length: T::EpochLength::get(),
			authorities: NextAuthorities::<T>::get().into_inner(),
			randomness: NextRandomness::<T>::get(),
			config: NextEpochConfig::<T>::get().unwrap_or_else(|| EpochConfig::<T>::get()),
		}
	}

	/// Fetch expected ticket-id for the given slot according to an "outside-in" sorting strategy.
	///
	/// Given an ordered sequence of tickets [t0, t1, t2, ..., tk] to be assigned to n slots,
	/// with n >= k, then the tickets are assigned to the slots according to the following
	/// strategy:
	///
	/// slot-index  : [ 0,  1,  2, ............ , n ]
	/// tickets     : [ t1, t3, t5, ... , t4, t2, t0 ].
	///
	/// With slot-index computed as `epoch_start() - slot`.
	///
	/// If `slot` value falls within the current epoch then we fetch tickets from the current epoch
	/// tickets list.
	///
	/// If `slot` value falls within the next epoch then we fetch tickets from the next epoch
	/// tickets ids list. Note that in this case we may have not finished receiving all the tickets
	/// for that epoch yet. The next epoch tickets should be considered "stable" only after the
	/// current epoch first half slots were elapsed (see `submit_tickets_unsigned_extrinsic`).
	///
	/// Returns `None` if, according to the sorting strategy, there is no ticket associated to the
	/// specified slot-index (happens if a ticket falls in the middle of an epoch and n > k),
	/// or if the slot falls beyond the next epoch.
	///
	/// Before importing the first block this returns `None`.
	pub fn slot_ticket_id(slot: Slot) -> Option<TicketId> {
		if frame_system::Pallet::<T>::block_number().is_zero() {
			return None;
		}
		let epoch_idx = EpochIndex::<T>::get();
		let epoch_len = T::EpochLength::get();
		let mut slot_idx = Self::slot_index(slot);
		let mut metadata = TicketsMeta::<T>::get();

		let get_ticket_idx = |slot_idx| {
			let ticket_idx = if slot_idx < epoch_len / 2 {
				2 * slot_idx + 1
			} else {
				2 * (epoch_len - (slot_idx + 1))
			};
			debug!(
				target: LOG_TARGET,
				"slot-idx {} <-> ticket-idx {}",
				slot_idx,
				ticket_idx
			);
			ticket_idx as u32
		};

		let mut epoch_tag = (epoch_idx & 1) as u8;

		if epoch_len <= slot_idx && slot_idx < 2 * epoch_len {
			// Try to get a ticket for the next epoch. Since its state values were not enacted yet,
			// we may have to finish sorting the tickets.
			epoch_tag ^= 1;
			slot_idx -= epoch_len;
			if metadata.unsorted_tickets_count != 0 {
				Self::sort_segments(u32::MAX, epoch_tag, &mut metadata);
				TicketsMeta::<T>::set(metadata);
			}
		} else if slot_idx >= 2 * epoch_len {
			return None;
		}

		let ticket_idx = get_ticket_idx(slot_idx);
		if ticket_idx < metadata.tickets_count[epoch_tag as usize] {
			TicketsIds::<T>::get((epoch_tag, ticket_idx))
		} else {
			None
		}
	}

	/// Returns ticket id and data associated with the given `slot`.
	///
	/// Refer to the `slot_ticket_id` documentation for the slot-ticket association
	/// criteria.
	pub fn slot_ticket(slot: Slot) -> Option<(TicketId, TicketBody)> {
		Self::slot_ticket_id(slot).and_then(|id| TicketsData::<T>::get(id).map(|body| (id, body)))
	}

	// Sort and truncate candidate tickets, cleanup storage.
	fn sort_and_truncate(candidates: &mut Vec<u128>, max_tickets: usize) -> u128 {
		candidates.sort_unstable();
		candidates.drain(max_tickets..).for_each(TicketsData::<T>::remove);
		candidates[max_tickets - 1]
	}

	/// Sort the tickets which belong to the epoch with the specified `epoch_tag`.
	///
	/// At most `max_segments` are taken from the `UnsortedSegments` structure.
	///
	/// The tickets of the removed segments are merged with the tickets on the `SortedCandidates`
	/// which is then sorted an truncated to contain at most `MaxTickets` entries.
	///
	/// If all the entries in `UnsortedSegments` are consumed, then `SortedCandidates` is elected
	/// as the next epoch tickets, else it is saved to be used by next calls of this function.
	pub(crate) fn sort_segments(max_segments: u32, epoch_tag: u8, metadata: &mut TicketsMetadata) {
		let unsorted_segments_count = metadata.unsorted_tickets_count.div_ceil(SEGMENT_MAX_SIZE);
		let max_segments = max_segments.min(unsorted_segments_count);
		let max_tickets = Self::epoch_length() as usize;

		// Fetch the sorted candidates (if any).
		let mut candidates = SortedCandidates::<T>::take().into_inner();

		// There is an upper bound to check only if we already sorted the max number
		// of allowed tickets.
		let mut upper_bound = *candidates.get(max_tickets - 1).unwrap_or(&TicketId::MAX);

		let mut require_sort = false;

		// Consume at most `max_segments` segments.
		// During the process remove every stale ticket from `TicketsData` storage.
		for segment_idx in (0..unsorted_segments_count).rev().take(max_segments as usize) {
			let segment = UnsortedSegments::<T>::take(segment_idx);
			metadata.unsorted_tickets_count -= segment.len() as u32;

			// Push only ids with a value less than the current `upper_bound`.
			let prev_len = candidates.len();
			for ticket_id in segment {
				if ticket_id < upper_bound {
					candidates.push(ticket_id);
				} else {
					TicketsData::<T>::remove(ticket_id);
				}
			}
			require_sort = candidates.len() != prev_len;

			// As we approach the tail of the segments buffer the `upper_bound` value is expected
			// to decrease (fast). We thus expect the number of tickets pushed into the
			// `candidates` vector to follow an exponential drop.
			//
			// Given this, sorting and truncating after processing each segment may be an overkill
			// as we may find pushing few tickets more and more often. Is preferable to perform
			// the sort and truncation operations only when we reach some bigger threshold
			// (currently set as twice the capacity of `SortCandidate`).
			//
			// The more is the protocol's redundancy factor (i.e. the ratio between tickets allowed
			// to be submitted and the epoch length) the more this check becomes relevant.
			if candidates.len() > 2 * max_tickets {
				upper_bound = Self::sort_and_truncate(&mut candidates, max_tickets);
				require_sort = false;
			}
		}

		if candidates.len() > max_tickets {
			Self::sort_and_truncate(&mut candidates, max_tickets);
		} else if require_sort {
			candidates.sort_unstable();
		}

		if metadata.unsorted_tickets_count == 0 {
			// Sorting is over, write to next epoch map.
			candidates.iter().enumerate().for_each(|(i, id)| {
				TicketsIds::<T>::insert((epoch_tag, i as u32), id);
			});
			metadata.tickets_count[epoch_tag as usize] = candidates.len() as u32;
		} else {
			// Keep the partial result for the next calls.
			SortedCandidates::<T>::set(BoundedVec::truncate_from(candidates));
		}
	}

	/// Append a set of tickets to the segments map.
	pub(crate) fn append_tickets(mut tickets: BoundedVec<TicketId, EpochLengthFor<T>>) {
		debug!(target: LOG_TARGET, "Appending batch with {} tickets", tickets.len());
		tickets.iter().for_each(|t| trace!(target: LOG_TARGET, "  + {t:032x}"));

		let mut metadata = TicketsMeta::<T>::get();
		let mut segment_idx = metadata.unsorted_tickets_count / SEGMENT_MAX_SIZE;

		while !tickets.is_empty() {
			let rem = metadata.unsorted_tickets_count % SEGMENT_MAX_SIZE;
			let to_be_added = tickets.len().min((SEGMENT_MAX_SIZE - rem) as usize);

			let mut segment = UnsortedSegments::<T>::get(segment_idx);
			let _ = segment
				.try_extend(tickets.drain(..to_be_added))
				.defensive_proof("We don't add more than `SEGMENT_MAX_SIZE` and this is the maximum bound for the vector.");
			UnsortedSegments::<T>::insert(segment_idx, segment);

			metadata.unsorted_tickets_count += to_be_added as u32;
			segment_idx += 1;
		}

		TicketsMeta::<T>::set(metadata);
	}

	/// Remove all tickets related data.
	///
	/// May not be efficient as the calling places may repeat some of this operations
	/// but is a very extraordinary operation (hopefully never happens in production)
	/// and better safe than sorry.
	fn reset_tickets_data() {
		let metadata = TicketsMeta::<T>::get();

		// Remove even/odd-epoch data.
		for epoch_tag in 0..=1 {
			for idx in 0..metadata.tickets_count[epoch_tag] {
				if let Some(id) = TicketsIds::<T>::get((epoch_tag as u8, idx)) {
					TicketsData::<T>::remove(id);
				}
			}
		}

		// Remove all unsorted tickets segments.
		let segments_count = metadata.unsorted_tickets_count.div_ceil(SEGMENT_MAX_SIZE);
		(0..segments_count).for_each(UnsortedSegments::<T>::remove);

		// Reset sorted candidates
		SortedCandidates::<T>::kill();

		// Reset tickets metadata
		TicketsMeta::<T>::kill();
	}

	/// Submit next epoch validator tickets via an unsigned extrinsic constructed with a call to
	/// `submit_unsigned_transaction`.
	///
	/// The submitted tickets are added to the next epoch outstanding tickets as long as the
	/// extrinsic is called within the first half of the epoch. Tickets received during the
	/// second half are dropped.
	pub fn submit_tickets_unsigned_extrinsic(tickets: Vec<TicketEnvelope>) -> bool {
		let tickets = BoundedVec::truncate_from(tickets);
		let call = Call::submit_tickets { tickets };
		let xt = T::create_bare(call.into());
		match SubmitTransaction::<T, Call<T>>::submit_transaction(xt) {
			Ok(_) => true,
			Err(e) => {
				error!(target: LOG_TARGET, "Error submitting tickets {:?}", e);
				false
			},
		}
	}

	/// Epoch length
	pub fn epoch_length() -> u32 {
		T::EpochLength::get()
	}

	/// Get current epoch index.
	pub fn epoch_index() -> u64 {
		EpochIndex::<T>::get()
	}

	/// Get current epoch authorities.
	pub fn authorities() -> Vec<AuthorityId> {
		Authorities::<T>::get().into_inner()
	}

	/// Get next epoch authorities.
	pub fn next_authorities() -> Vec<AuthorityId> {
		NextAuthorities::<T>::get().into_inner()
	}

	/// Get genesis slot.
	pub fn genesis_slot() -> Slot {
		GenesisSlot::<T>::get()
	}

	/// Get current slot.
	pub fn current_slot() -> Slot {
		CurrentSlot::<T>::get()
	}

	/// Get current epoch randomness.
	pub fn randomness() -> Randomness {
		CurrentRandomness::<T>::get()
	}

	/// Get next epoch randomness.
	pub fn next_randomness() -> Randomness {
		NextRandomness::<T>::get()
	}

	/// Get randomness accumulator.
	pub fn randomness_accumulator() -> Randomness {
		RandomnessAccumulator::<T>::get()
	}

	/// Get current epoch configuration.
	pub fn config() -> EpochConfiguration {
		EpochConfig::<T>::get()
	}

	/// Get next epoch configuration.
	pub fn next_config() -> Option<EpochConfiguration> {
		NextEpochConfig::<T>::get()
	}

	/// Get ring context.
	pub fn ring_context() -> Option<vrf::RingContext> {
		RingContext::<T>::get()
	}
}

/// Trigger an epoch change, if any should take place.
pub trait EpochChangeTrigger {
	/// May trigger an epoch change, if any should take place.
	///
	/// Returns an optional `Weight` if epoch change has been triggered.
	///
	/// This should be called during every block, after initialization is done.
	fn trigger<T: Config>(_: BlockNumberFor<T>) -> Weight;
}

/// An `EpochChangeTrigger` which does nothing.
///
/// In practice this means that the epoch change logic is left to some external component
/// (e.g. pallet-session).
pub struct EpochChangeExternalTrigger;

impl EpochChangeTrigger for EpochChangeExternalTrigger {
	fn trigger<T: Config>(_: BlockNumberFor<T>) -> Weight {
		// nothing - trigger is external.
		Weight::zero()
	}
}

/// An `EpochChangeTrigger` which recycle the same authorities set forever.
///
/// The internal trigger should only be used when no other module is responsible for
/// changing authority set.
pub struct EpochChangeInternalTrigger;

impl EpochChangeTrigger for EpochChangeInternalTrigger {
	fn trigger<T: Config>(block_num: BlockNumberFor<T>) -> Weight {
		if Pallet::<T>::should_end_epoch(block_num) {
			let authorities = NextAuthorities::<T>::get();
			let next_authorities = authorities.clone();
			let len = next_authorities.len() as u32;
			Pallet::<T>::enact_epoch_change(authorities, next_authorities);
			T::WeightInfo::enact_epoch_change(len, T::EpochLength::get())
		} else {
			Weight::zero()
		}
	}
}

impl<T: Config> BoundToRuntimeAppPublic for Pallet<T> {
	type Public = AuthorityId;
}
