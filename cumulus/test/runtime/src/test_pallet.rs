// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

/// A special pallet that exposes dispatchables that are only useful for testing.
pub use pallet::*;

use codec::Encode;

/// Some key that we set in genesis and only read in
/// [`SingleBlockMigrations`](crate::SingleBlockMigrations) to ensure that
/// [`OnRuntimeUpgrade`](frame_support::traits::OnRuntimeUpgrade) works as expected.
pub const TEST_RUNTIME_UPGRADE_KEY: &[u8] = b"+test_runtime_upgrade_key+";

/// Generates the storage key for Alice's account on the relay chain.
pub fn relay_alice_account_key() -> alloc::vec::Vec<u8> {
	use sp_keyring::Sr25519Keyring;

	let alice = Sr25519Keyring::Alice.to_account_id();

	let mut key = sp_io::hashing::twox_128(b"System").to_vec();
	key.extend_from_slice(&sp_io::hashing::twox_128(b"Account"));
	key.extend_from_slice(&sp_io::hashing::blake2_128(&alice.encode()));
	key.extend_from_slice(&alice.encode());
	key
}

#[frame_support::pallet(dev_mode)]
pub mod pallet {
	use crate::test_pallet::TEST_RUNTIME_UPGRADE_KEY;
	use alloc::{vec, vec::Vec};
	use cumulus_primitives_core::{CumulusDigestItem, ParaId, XcmpMessageSource};
	use cumulus_primitives_storage_weight_reclaim::get_proof_size;
	use frame_support::{
		dispatch::DispatchInfo,
		inherent::{InherentData, InherentIdentifier, ProvideInherent},
		pallet_prelude::*,
		traits::IsSubType,
		weights::constants::WEIGHT_REF_TIME_PER_SECOND,
		DebugNoBound,
	};
	use frame_system::pallet_prelude::*;
	use sp_runtime::traits::{Dispatchable, Implication, TransactionExtension};

	/// The inherent identifier for weight consumption.
	pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"consume0";

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config + cumulus_pallet_parachain_system::Config {}

	/// A simple storage map for testing purposes.
	#[pallet::storage]
	pub type TestMap<T: Config> = StorageMap<_, Twox64Concat, u32, (), ValueQuery>;

	/// Pending outbound HRMP messages queued by test extrinsics.
	#[pallet::storage]
	pub type PendingOutboundHrmpMessages<T: Config> =
		StorageValue<_, alloc::vec::Vec<(ParaId, alloc::vec::Vec<u8>)>, ValueQuery>;

	impl<T: Config> XcmpMessageSource for Pallet<T> {
		fn take_outbound_messages(
			maximum_channels: usize,
			excluded_recipients: &[ParaId],
		) -> alloc::vec::Vec<(ParaId, alloc::vec::Vec<u8>)> {
			PendingOutboundHrmpMessages::<T>::mutate(|messages| {
				let mut taken_recipients = alloc::vec::Vec::new();
				let mut result = alloc::vec::Vec::new();
				messages.retain(|(recipient, data)| {
					if result.len() >= maximum_channels ||
						excluded_recipients.contains(recipient) ||
						taken_recipients.contains(recipient)
					{
						return true;
					}
					taken_recipients.push(*recipient);
					result.push((*recipient, data.clone()));
					false
				});
				result
			})
		}
	}

	/// When active, `on_initialize` queues one HRMP message per block, alternating
	/// between `HRMP_RECIPIENT_HIGH` (odd blocks) and `HRMP_RECIPIENT_LOW` (even blocks).
	/// This produces descending recipient order across consecutive blocks in a bundle,
	/// exercising the HRMP message sorting in the collation path.
	#[pallet::storage]
	pub type HrmpSendingActive<T: Config> = StorageValue<_, bool, ValueQuery>;

	/// Flag to indicate if a 1s weight should be registered in the next `on_initialize`.
	#[pallet::storage]
	pub type ScheduleWeightRegistration<T: Config> = StorageValue<_, bool, ValueQuery>;

	/// Weight to be consumed by the inherent call.
	#[pallet::storage]
	pub type InherentWeightConsume<T: Config> = StorageValue<_, Weight, OptionQuery>;

	/// A map that contains on single big value at the current block.
	///
	/// In every block we are moving the big value from the previous block to current block. This is
	/// done to test that the storage proof size between multiple blocks in the same bundle is
	/// shared.
	#[pallet::storage]
	pub type BigValueMove<T: Config> =
		StorageMap<_, Twox64Concat, BlockNumberFor<T>, Vec<u8>, OptionQuery>;

	pub const HRMP_RECIPIENT_LOW: u32 = 2500;
	pub const HRMP_RECIPIENT_HIGH: u32 = 2600;

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(n: BlockNumberFor<T>) -> Weight {
			if HrmpSendingActive::<T>::get() {
				let block_num: u32 = n.try_into().unwrap_or(0);
				let recipient = if block_num % 2 == 1 {
					ParaId::from(HRMP_RECIPIENT_HIGH)
				} else {
					ParaId::from(HRMP_RECIPIENT_LOW)
				};
				PendingOutboundHrmpMessages::<T>::mutate(|messages| {
					messages.push((recipient, vec![block_num as u8]));
				});
			}

			if ScheduleWeightRegistration::<T>::get() {
				let weight_to_register = Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 0);

				let left_weight = frame_system::Pallet::<T>::remaining_block_weight();

				if left_weight.can_consume(weight_to_register) {
					tracing::info!("Consuming 1s of weight :)");
					// We have enough capacity, consume the flag and register the weight
					ScheduleWeightRegistration::<T>::kill();
					return weight_to_register;
				}
			}

			if let Some(mut value) = BigValueMove::<T>::take(n - 1u32.into()) {
				// Modify the value a little bit.
				let parent_hash = frame_system::Pallet::<T>::parent_hash();
				value[..parent_hash.as_ref().len()].copy_from_slice(parent_hash.as_ref());

				BigValueMove::<T>::insert(n, value);

				// Depositing the event is important, because then we write the actual proof size
				// into the state. If some node returns a different proof size on import of this
				// block, we will detect it this way as the storage root will be different.
				Self::deposit_event(Event::MovedBigValue {
					proof_size: get_proof_size().unwrap_or_default(),
				})
			}

			Weight::zero()
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// A test dispatchable for setting a custom head data in `validate_block`.
		#[pallet::weight(0)]
		pub fn set_custom_validation_head_data(
			_: OriginFor<T>,
			custom_header: alloc::vec::Vec<u8>,
		) -> DispatchResult {
			cumulus_pallet_parachain_system::Pallet::<T>::set_custom_validation_head_data(
				custom_header,
			);
			Ok(())
		}

		/// A dispatchable that first reads two values from two different child tries, asserts they
		/// are the expected values (if the values exist in the state) and then writes two different
		/// values to these child tries.
		#[pallet::weight(0)]
		pub fn read_and_write_child_tries(_: OriginFor<T>) -> DispatchResult {
			let key = &b"hello"[..];
			let first_trie = &b"first"[..];
			let second_trie = &b"second"[..];
			let first_value = "world1".encode();
			let second_value = "world2".encode();

			if let Some(res) = sp_io::default_child_storage::get(first_trie, key) {
				assert_eq!(first_value, res);
			}
			if let Some(res) = sp_io::default_child_storage::get(second_trie, key) {
				assert_eq!(second_value, res);
			}

			sp_io::default_child_storage::set(first_trie, key, &first_value);
			sp_io::default_child_storage::set(second_trie, key, &second_value);

			Ok(())
		}

		/// Reads a key and writes a big value under this key.
		///
		/// At genesis this `key` is empty and thus, will only be set in consequent blocks.
		pub fn read_and_write_big_value(_: OriginFor<T>) -> DispatchResult {
			let key = &b"really_huge_value"[..];
			sp_io::storage::get(key);
			sp_io::storage::set(key, &vec![0u8; 1024 * 1024 * 5]);

			Ok(())
		}

		/// Stores `()` in `TestMap` for keys from 0 up to `max_key`.
		#[pallet::weight(0)]
		pub fn store_values_in_map(_: OriginFor<T>, max_key: u32) -> DispatchResult {
			for i in 0..=max_key {
				TestMap::<T>::insert(i, ());
			}
			Ok(())
		}

		/// Removes the value associated with `key` from `TestMap`.
		#[pallet::weight(0)]
		pub fn remove_value_from_map(_: OriginFor<T>, key: u32) -> DispatchResult {
			TestMap::<T>::remove(key);
			Ok(())
		}

		/// Directly sets `n` small UMP messages in `PendingUpwardMessages`.
		#[pallet::weight(0)]
		pub fn send_n_upward_messages(_: OriginFor<T>, n: u32) -> DispatchResult {
			let messages: alloc::vec::Vec<_> = (0..n).map(|i| vec![(i % 256) as u8]).collect();
			cumulus_pallet_parachain_system::PendingUpwardMessages::<T>::put(messages);
			Ok(())
		}

		/// Sends a UMP message of specific size (in bytes).
		#[pallet::weight(0)]
		pub fn send_upward_message_of_size(_: OriginFor<T>, size: u32) -> DispatchResult {
			let message = alloc::vec![0u8; size as usize];
			cumulus_pallet_parachain_system::Pallet::<T>::send_upward_message(message)
				.map_err(|_| "Failed to send upward message")?;
			Ok(())
		}

		/// Queues `n` small HRMP messages to `recipient`.
		#[pallet::weight(0)]
		pub fn queue_hrmp_messages(_: OriginFor<T>, n: u32, recipient: ParaId) -> DispatchResult {
			PendingOutboundHrmpMessages::<T>::mutate(|messages| {
				for i in 0..n {
					messages.push((recipient, vec![(i % 256) as u8]));
				}
			});
			Ok(())
		}

		/// Queues one HRMP message each to `n` consecutive recipients starting from
		/// `first_recipient`.
		#[pallet::weight(0)]
		pub fn queue_hrmp_messages_to_n_recipients(
			_: OriginFor<T>,
			n: u32,
			first_recipient: ParaId,
		) -> DispatchResult {
			PendingOutboundHrmpMessages::<T>::mutate(|messages| {
				for i in 0..n {
					messages.push((ParaId::from(u32::from(first_recipient) + i), vec![i as u8]));
				}
			});
			Ok(())
		}

		/// Schedule a 1 second weight registration in the next `on_initialize`.
		#[pallet::weight(0)]
		pub fn schedule_weight_registration(_: OriginFor<T>) -> DispatchResult {
			ScheduleWeightRegistration::<T>::set(true);
			Ok(())
		}

		/// Set the weight to be consumed by the next inherent call.
		#[pallet::weight(0)]
		pub fn set_inherent_weight_consume(_: OriginFor<T>, weight: Weight) -> DispatchResult {
			InherentWeightConsume::<T>::put(weight);
			Ok(())
		}

		/// Consume weight via inherent call (clears the storage after consuming).
		#[pallet::weight((
			InherentWeightConsume::<T>::get().unwrap_or_default(),
			DispatchClass::Mandatory
		))]
		pub fn consume_weight_inherent(origin: OriginFor<T>) -> DispatchResult {
			ensure_none(origin)?;

			// Clear the storage item to ensure this can only be called once per inherent
			InherentWeightConsume::<T>::kill();

			Ok(())
		}

		/// This function registers a high weight usage manually, while it actually only announces
		/// to use a weight of `0` :)
		///
		/// Uses the [`TestTransactionExtension`] logic to ensure the transaction is only accepted
		/// when we can fit the `1s` weight into the block.
		#[pallet::weight(0)]
		pub fn use_more_weight_than_announced(
			_: OriginFor<T>,
			_must_be_first_block_in_core: bool,
		) -> DispatchResult {
			// Register weight manually.
			frame_system::Pallet::<T>::register_extra_weight_unchecked(
				Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 0),
				DispatchClass::Normal,
			);

			Ok(())
		}

		/// Deposits the `UseFullCore` digest item to signal that this block should use the full
		/// core.
		#[pallet::weight(0)]
		pub fn set_use_full_core(_: OriginFor<T>) -> DispatchResult {
			frame_system::Pallet::<T>::deposit_log(CumulusDigestItem::UseFullCore.to_digest_item());
			Ok(())
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = sp_inherents::MakeFatalError<()>;
		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn create_inherent(_data: &InherentData) -> Option<Self::Call> {
			// Check if there's weight to consume from storage
			let weight_to_consume = InherentWeightConsume::<T>::get()?;

			// Check if the weight fits in the remaining block capacity
			let remaining_weight = frame_system::Pallet::<T>::remaining_block_weight();

			if remaining_weight.can_consume(weight_to_consume) {
				Some(Call::consume_weight_inherent {})
			} else {
				// Weight doesn't fit, don't create the inherent
				None
			}
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::consume_weight_inherent {})
		}
	}

	#[derive(frame_support::DefaultNoBound)]
	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		#[serde(skip)]
		pub _config: core::marker::PhantomData<T>,
		/// Controls if the `BigValueMove` logic is enabled.
		pub enable_big_value_move: bool,
		/// Activate HRMP sending with descending recipients from genesis.
		pub enable_hrmp_sending: bool,
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			sp_io::storage::set(TEST_RUNTIME_UPGRADE_KEY, &[1, 2, 3, 4]);

			if self.enable_big_value_move {
				BigValueMove::<T>::insert(BlockNumberFor::<T>::from(0u32), vec![0u8; 4 * 1024]);
			}

			if self.enable_hrmp_sending {
				HrmpSendingActive::<T>::set(true);
			}
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		MovedBigValue { proof_size: u64 },
	}

	#[derive(
		DebugNoBound,
		Encode,
		Decode,
		CloneNoBound,
		EqNoBound,
		PartialEqNoBound,
		TypeInfo,
		DecodeWithMemTracking,
	)]
	#[scale_info(skip_type_params(T))]
	pub struct TestTransactionExtension<T>(core::marker::PhantomData<T>);

	impl<T> Default for TestTransactionExtension<T> {
		fn default() -> Self {
			Self(core::marker::PhantomData)
		}
	}

	impl<T: Config> TransactionExtension<T::RuntimeCall> for TestTransactionExtension<T>
	where
		T: Config + Send + Sync,
		T::RuntimeCall: IsSubType<Call<T>> + Dispatchable<Info = DispatchInfo>,
	{
		const IDENTIFIER: &'static str = "TestTransactionExtension";
		type Implicit = ();
		type Val = ();
		type Pre = ();

		fn validate(
			&self,
			origin: T::RuntimeOrigin,
			call: &T::RuntimeCall,
			_info: &DispatchInfo,
			_len: usize,
			_self_implicit: Self::Implicit,
			_inherited_implication: &impl Implication,
			_: TransactionSource,
		) -> ValidateResult<Self::Val, T::RuntimeCall> {
			if let Some(call) = call.is_sub_type() {
				match call {
					Call::use_more_weight_than_announced { must_be_first_block_in_core } => {
						if {
							let digest = frame_system::Pallet::<T>::digest();

							CumulusDigestItem::find_block_bundle_info(&digest)
								// Default being `true` to support `validate_transaction`
								.map_or(true, |bi| {
									// Either we want that the transaction goes into the first block
									// of a core
									bi.index == 0 && *must_be_first_block_in_core ||
										// Or it goes to any block that isn't the first block
										bi.index > 0 && !*must_be_first_block_in_core
								})
						} {
							Ok((
								ValidTransaction {
									provides: vec![vec![1, 2, 3, 4, 5]],
									..Default::default()
								},
								(),
								origin,
							))
						} else {
							Err(TransactionValidityError::Invalid(
								InvalidTransaction::ExhaustsResources,
							))
						}
					},
					_ => Ok((Default::default(), (), origin)),
				}
			} else {
				Ok((Default::default(), (), origin))
			}
		}

		fn prepare(
			self,
			val: Self::Val,
			_origin: &T::RuntimeOrigin,
			_call: &T::RuntimeCall,
			_info: &DispatchInfo,
			_len: usize,
		) -> Result<Self::Pre, TransactionValidityError> {
			Ok(val)
		}

		fn weight(&self, _: &T::RuntimeCall) -> Weight {
			Weight::zero()
		}
	}
}

impl<T: Config> cumulus_pallet_parachain_system::OnSystemEvent for Pallet<T> {
	fn on_validation_data(_data: &cumulus_primitives_core::PersistedValidationData) {
		// Nothing to do here for tests
	}

	fn on_validation_code_applied() {
		// Nothing to do here for tests
	}

	fn on_relay_state_proof(
		relay_state_proof: &cumulus_pallet_parachain_system::relay_state_snapshot::RelayChainStateProof,
	) -> frame_support::weights::Weight {
		use crate::{Balance, Nonce};
		use frame_system::AccountInfo;
		use pallet_balances::AccountData;

		let alice_key = crate::test_pallet::relay_alice_account_key();

		// Verify that Alice's account is included in the relay proof.
		relay_state_proof
			.read_optional_entry::<AccountInfo<Nonce, AccountData<Balance>>>(&alice_key)
			.expect("Invalid relay chain state proof");

		frame_support::weights::Weight::zero()
	}
}
