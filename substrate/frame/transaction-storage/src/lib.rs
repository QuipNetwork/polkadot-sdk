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

//! Transaction storage pallet. Indexes transactions and manages storage proofs.

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

mod benchmarking;
pub mod weights;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

extern crate alloc;

use alloc::vec::Vec;
use codec::{Decode, Encode, MaxEncodedLen};
use core::fmt::Debug;
use frame_support::{
	dispatch::GetDispatchInfo,
	pallet_prelude::InvalidTransaction,
	traits::{
		fungible::{hold::Balanced, Credit, Inspect, Mutate, MutateHold},
		OnUnbalanced,
	},
};
use frame_system::pallet_prelude::BlockNumberFor;
use sp_runtime::traits::{BlakeTwo256, Dispatchable, Hash, One, Saturating, Zero};
use sp_transaction_storage_proof::{
	encode_index, num_chunks, random_chunk, ChunkIndex, InherentError, TransactionStorageProof,
	CHUNK_SIZE, INHERENT_IDENTIFIER,
};

/// A type alias for the balance type from this pallet's point of view.
type BalanceOf<T> =
	<<T as Config>::Currency as Inspect<<T as frame_system::Config>::AccountId>>::Balance;
pub type CreditOf<T> = Credit<<T as frame_system::Config>::AccountId, <T as Config>::Currency>;

// Re-export pallet items so that they can be accessed from the crate namespace.
pub use pallet::*;
pub use weights::WeightInfo;

/// Default retention period for data (in blocks).
pub const DEFAULT_RETENTION_PERIOD: u32 = 100800;

// TODO: https://github.com/paritytech/polkadot-bulletin-chain/issues/139 - Clarify purpose of allocator limits and decide whether to remove or use these constants.
/// Maximum bytes that can be stored in one transaction.
// Setting higher limit also requires raising the allocator limit.
pub const DEFAULT_MAX_TRANSACTION_SIZE: u32 = 8 * 1024 * 1024;
pub const DEFAULT_MAX_BLOCK_TRANSACTIONS: u32 = 512;

/// Encountered an impossible situation, implies a bug.
pub const IMPOSSIBLE: InvalidTransaction = InvalidTransaction::Custom(0);
/// Data size is not in the allowed range.
pub const BAD_DATA_SIZE: InvalidTransaction = InvalidTransaction::Custom(1);
/// Renewed extrinsic not found.
pub const RENEWED_NOT_FOUND: InvalidTransaction = InvalidTransaction::Custom(2);
/// Authorization was not found.
pub const AUTHORIZATION_NOT_FOUND: InvalidTransaction = InvalidTransaction::Custom(3);
/// Authorization has not expired.
pub const AUTHORIZATION_NOT_EXPIRED: InvalidTransaction = InvalidTransaction::Custom(4);

/// Number of transactions and bytes covered by an authorization.
#[derive(PartialEq, Eq, Debug, Encode, Decode, scale_info::TypeInfo, MaxEncodedLen)]
pub struct AuthorizationExtent {
	/// Number of transactions.
	pub transactions: u32,
	/// Number of bytes.
	pub bytes: u64,
}

/// Hash of a stored blob of data.
type ContentHash = [u8; 32];

/// The scope of an authorization.
#[derive(Encode, Decode, scale_info::TypeInfo, MaxEncodedLen)]
enum AuthorizationScope<AccountId> {
	/// Authorization for the given account to store arbitrary data.
	Account(AccountId),
	/// Authorization for anyone to store data with a specific hash.
	Preimage(ContentHash),
}

type AuthorizationScopeFor<T> = AuthorizationScope<<T as frame_system::Config>::AccountId>;

/// An authorization to store data.
#[derive(Encode, Decode, scale_info::TypeInfo, MaxEncodedLen)]
struct Authorization<BlockNumber> {
	/// Extent of the authorization (number of transactions/bytes).
	extent: AuthorizationExtent,
	/// The block at which this authorization expires.
	expiration: BlockNumber,
}

type AuthorizationFor<T> = Authorization<BlockNumberFor<T>>;

/// State data for a stored transaction.
#[derive(Encode, Decode, Clone, Debug, PartialEq, Eq, scale_info::TypeInfo, MaxEncodedLen)]
pub struct TransactionInfo {
	/// Chunk trie root.
	chunk_root: <BlakeTwo256 as Hash>::Output,
	/// Plain hash of indexed data.
	content_hash: <BlakeTwo256 as Hash>::Output,
	/// Size of indexed data in bytes.
	size: u32,
	/// Total number of chunks added in the block with this transaction. This
	/// is used to find transaction info by block chunk index using binary search.
	///
	/// Cumulative value of all previous transactions in the block; the last transaction holds the
	/// total chunks.
	block_chunks: ChunkIndex,
}

impl TransactionInfo {
	/// Get the number of total chunks.
	///
	/// See the `block_chunks` field of [`TransactionInfo`] for details.
	pub fn total_chunks(txs: &[TransactionInfo]) -> ChunkIndex {
		txs.last().map_or(0, |t| t.block_chunks)
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	/// A reason for this pallet placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// The funds are held as deposit for the used storage.
		StorageFeeHold,
	}

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
		/// A dispatchable call.
		type RuntimeCall: Parameter
			+ Dispatchable<RuntimeOrigin = Self::RuntimeOrigin>
			+ GetDispatchInfo
			+ From<frame_system::Call<Self>>;
		/// The fungible type for this pallet.
		type Currency: Mutate<Self::AccountId>
			+ MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>
			+ Balanced<Self::AccountId>;
		/// The overarching runtime hold reason.
		type RuntimeHoldReason: From<HoldReason>;
		/// Handler for the unbalanced decrease when fees are burned.
		type FeeDestination: OnUnbalanced<CreditOf<Self>>;
		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;
		/// Maximum number of indexed transactions in the block.
		#[pallet::constant]
		type MaxBlockTransactions: Get<u32>;
		/// Maximum data set in a single transaction in bytes.
		#[pallet::constant]
		type MaxTransactionSize: Get<u32>;
	}

	#[pallet::error]
	pub enum Error<T> {
		/// Attempted to call `store`/`renew` outside of block execution.
		BadContext,
		/// Data size is not in the allowed range.
		BadDataSize,
		/// Too many transactions in the block.
		TooManyTransactions,
		/// Invalid configuration.
		NotConfigured,
		/// Renewed extrinsic is not found.
		RenewedNotFound,
		/// Proof was not expected in this block.
		UnexpectedProof,
		/// Proof failed verification.
		InvalidProof,
		/// Missing storage proof.
		MissingProof,
		/// Unable to verify proof because state data is missing.
		MissingStateData,
		/// Double proof check in the block.
		DoubleCheck,
		/// Storage proof was not checked in the block.
		ProofNotChecked,
		/// Authorization was not found.
		AuthorizationNotFound,
		/// Authorization has not expired.
		AuthorizationNotExpired,
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(n: BlockNumberFor<T>) -> Weight {
			// TODO: https://github.com/paritytech/polkadot-sdk/issues/10203 - Replace this with benchmarked weights.
			let mut weight = Weight::zero();
			let db_weight = T::DbWeight::get();

			// Drop obsolete roots. The proof for `obsolete` will be checked later
			// in this block, so we drop `obsolete` - 1.
			weight.saturating_accrue(db_weight.reads(1));
			let period = Self::retention_period();
			let obsolete = n.saturating_sub(period.saturating_add(One::one()));
			if obsolete > Zero::zero() {
				weight.saturating_accrue(db_weight.writes(1));
				Transactions::<T>::remove(obsolete);
			}

			// For `on_finalize`
			weight.saturating_accrue(db_weight.reads_writes(3, 1));
			weight
		}

		fn on_finalize(n: BlockNumberFor<T>) {
			assert!(
				ProofChecked::<T>::take() || {
					// Proof is not required for early or empty blocks.
					let number = frame_system::Pallet::<T>::block_number();
					let period = Self::retention_period();
					let target_number = number.saturating_sub(period);

					target_number.is_zero() || {
						// An empty block means no transactions were stored, relying on the fact
						// below that we store transactions only if they contain chunks.
						!Transactions::<T>::contains_key(target_number)
					}
				},
				"Storage proof must be checked once in the block"
			);

			// Insert new transactions, iff they have chunks.
			let transactions = BlockTransactions::<T>::take();
			let total_chunks = TransactionInfo::total_chunks(&transactions);
			if total_chunks != 0 {
				Transactions::<T>::insert(n, transactions);
			}
		}

		fn integrity_test() {
			assert!(
				!T::MaxBlockTransactions::get().is_zero(),
				"MaxTransactionSize must be greater than zero"
			);
			assert!(
				!T::MaxTransactionSize::get().is_zero(),
				"MaxTransactionSize must be greater than zero"
			);
			let default_period = DEFAULT_RETENTION_PERIOD.into();
			let retention_period = GenesisConfig::<T>::default().retention_period;
			assert_eq!(
				retention_period, default_period,
				"GenesisConfig.retention_period must match DEFAULT_RETENTION_PERIOD"
			);
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Index and store data off chain. Minimum data size is 1 byte, maximum is
		/// `MaxTransactionSize`. Data will be removed after `RetentionPeriod` blocks, unless
		/// `renew` is called.
		///
		/// Emits [`Stored`](Event::Stored) when successful.
		///
		/// ## Complexity
		///
		/// O(n*log(n)) of data size, as all data is pushed to an in-memory trie.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::store(data.len() as u32))]
		pub fn store(origin: OriginFor<T>, data: Vec<u8>) -> DispatchResult {
			// In the case of a regular unsigned transaction, this should have been checked by
			// pre_dispatch. In the case of a regular signed transaction, this should have been
			// checked by pre_dispatch_signed.
			Self::ensure_data_size_ok(data.len())?;
			let sender = ensure_signed(origin)?;
			Self::apply_fee(sender, data.len() as u32)?;

			// Chunk data and compute storage root
			let chunks: Vec<_> = data.chunks(CHUNK_SIZE).map(|c| c.to_vec()).collect();
			let chunk_count = chunks.len() as u32;
			debug_assert_eq!(chunk_count, num_chunks(data.len() as u32));
			let root = sp_io::trie::blake2_256_ordered_root(chunks, sp_runtime::StateVersion::V1);

			let content_hash = sp_io::hashing::blake2_256(&data);
			let extrinsic_index =
				frame_system::Pallet::<T>::extrinsic_index().ok_or(Error::<T>::BadContext)?;
			sp_io::transaction_index::index(extrinsic_index, data.len() as u32, content_hash);

			let mut index = 0;
			BlockTransactions::<T>::mutate(|transactions| {
				if transactions.len() + 1 > T::MaxBlockTransactions::get() as usize {
					return Err(Error::<T>::TooManyTransactions);
				}
				let total_chunks = TransactionInfo::total_chunks(&transactions) + chunk_count;
				index = transactions.len() as u32;
				transactions
					.try_push(TransactionInfo {
						chunk_root: root,
						size: data.len() as u32,
						content_hash: content_hash.into(),
						block_chunks: total_chunks,
					})
					.map_err(|_| Error::<T>::TooManyTransactions)
			})?;
			Self::deposit_event(Event::Stored { index, content_hash });
			Ok(())
		}

		/// Renew previously stored data. Parameters are the block number that contains previous
		/// `store` or `renew` call and transaction index within that block. Transaction index is
		/// emitted in the `Stored` or `Renewed` event. Applies same fees as `store`.
		///
		/// Emits [`Renewed`](Event::Renewed) when successful.
		///
		/// ## Complexity
		///
		/// O(1).
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::renew())]
		pub fn renew(
			origin: OriginFor<T>,
			block: BlockNumberFor<T>,
			index: u32,
		) -> DispatchResultWithPostInfo {
			let sender = ensure_signed(origin)?;
			let info = Self::transaction_info(block, index).ok_or(Error::<T>::RenewedNotFound)?;

			// In the case of a regular unsigned transaction, this should have been checked by
			// pre_dispatch. In the case of a regular signed transaction, this should have been
			// checked by pre_dispatch_signed.
			Self::ensure_data_size_ok(info.size as usize)?;

			let extrinsic_index =
				frame_system::Pallet::<T>::extrinsic_index().ok_or(Error::<T>::BadContext)?;
			Self::apply_fee(sender, info.size)?;
			let content_hash = info.content_hash.into();
			sp_io::transaction_index::renew(extrinsic_index, content_hash);

			let mut index = 0;
			BlockTransactions::<T>::mutate(|transactions| {
				if transactions.len() + 1 > T::MaxBlockTransactions::get() as usize {
					return Err(Error::<T>::TooManyTransactions);
				}
				let chunks = num_chunks(info.size);
				let total_chunks = TransactionInfo::total_chunks(&transactions) + chunks;
				index = transactions.len() as u32;
				transactions
					.try_push(TransactionInfo {
						chunk_root: info.chunk_root,
						size: info.size,
						content_hash: info.content_hash,
						block_chunks: total_chunks,
					})
					.map_err(|_| Error::<T>::TooManyTransactions)
			})?;
			Self::deposit_event(Event::Renewed { index, content_hash });
			Ok(().into())
		}

		/// Check storage proof for block number `block_number() - RetentionPeriod`. If such a block
		/// does not exist, the proof is expected to be `None`.
		///
		/// ## Complexity
		///
		/// Linear w.r.t the number of indexed transactions in the proved block for random probing.
		/// There's a DB read for each transaction.
		#[pallet::call_index(2)]
		#[pallet::weight((T::WeightInfo::check_proof_max(), DispatchClass::Mandatory))]
		pub fn check_proof(
			origin: OriginFor<T>,
			proof: TransactionStorageProof,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;
			ensure!(!ProofChecked::<T>::get(), Error::<T>::DoubleCheck);

			// Get the target block metadata.
			let number = frame_system::Pallet::<T>::block_number();
			let period = Self::retention_period();
			let target_number = number.saturating_sub(period);
			ensure!(!target_number.is_zero(), Error::<T>::UnexpectedProof);
			let transactions =
				Transactions::<T>::get(target_number).ok_or(Error::<T>::MissingStateData)?;

			// Verify the proof with a "random" chunk (randomness is based on the parent hash).
			let parent_hash = frame_system::Pallet::<T>::parent_hash();
			Self::verify_chunk_proof(proof, parent_hash.as_ref(), transactions.to_vec())?;
			ProofChecked::<T>::put(true);
			Self::deposit_event(Event::ProofChecked);
			Ok(().into())
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Stored data under specified index.
		Stored { index: u32, content_hash: ContentHash },
		/// Renewed data under specified index.
		Renewed { index: u32, content_hash: ContentHash },
		/// Storage proof was successfully checked.
		ProofChecked,
		/// An account `who` was authorized to store `bytes` bytes in `transactions` transactions.
		AccountAuthorized { who: T::AccountId, transactions: u32, bytes: u64 },
		/// An authorization for account `who` was refreshed.
		AccountAuthorizationRefreshed { who: T::AccountId },
		/// Authorization was given for a preimage of `content_hash` (not exceeding `max_size`) to
		/// be stored by anyone.
		PreimageAuthorized { content_hash: ContentHash, max_size: u64 },
		/// An authorization for a preimage of `content_hash` was refreshed.
		PreimageAuthorizationRefreshed { content_hash: ContentHash },
		/// An expired account authorization was removed.
		ExpiredAccountAuthorizationRemoved { who: T::AccountId },
		/// An expired preimage authorization was removed.
		ExpiredPreimageAuthorizationRemoved { content_hash: ContentHash },
	}

	/// Authorizations, keyed by scope.
	#[pallet::storage]
	pub(super) type Authorizations<T: Config> =
		StorageMap<_, Blake2_128Concat, AuthorizationScopeFor<T>, AuthorizationFor<T>, OptionQuery>;

	/// Collection of transaction metadata by block number.
	#[pallet::storage]
	pub type Transactions<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		BlockNumberFor<T>,
		BoundedVec<TransactionInfo, T::MaxBlockTransactions>,
		OptionQuery,
	>;

	#[pallet::storage]
	/// Storage fee per byte.
	pub type ByteFee<T: Config> = StorageValue<_, BalanceOf<T>>;

	#[pallet::storage]
	/// Storage fee per transaction.
	pub type EntryFee<T: Config> = StorageValue<_, BalanceOf<T>>;

	/// Number of blocks for which stored data must be retained.
	///
	/// Data older than `RetentionPeriod` blocks is eligible for removal unless it
	/// has been explicitly renewed. Validators are required to prove possession of
	/// data corresponding to block `N - RetentionPeriod` when producing block `N`.
	#[pallet::storage]
	pub type RetentionPeriod<T: Config> = StorageValue<_, BlockNumberFor<T>, ValueQuery>;

	// Intermediates
	#[pallet::storage]
	pub type BlockTransactions<T: Config> =
		StorageValue<_, BoundedVec<TransactionInfo, T::MaxBlockTransactions>, ValueQuery>;

	/// Was the proof checked in this block?
	#[pallet::storage]
	pub type ProofChecked<T: Config> = StorageValue<_, bool, ValueQuery>;

	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub byte_fee: BalanceOf<T>,
		pub entry_fee: BalanceOf<T>,
		pub retention_period: BlockNumberFor<T>,
	}

	impl<T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			Self {
				byte_fee: 10u32.into(),
				entry_fee: 1000u32.into(),
				retention_period: DEFAULT_RETENTION_PERIOD.into(),
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			ByteFee::<T>::put(self.byte_fee);
			EntryFee::<T>::put(self.entry_fee);
			RetentionPeriod::<T>::put(self.retention_period);
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = InherentError;
		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn create_inherent(data: &InherentData) -> Option<Self::Call> {
			let proof = data
				.get_data::<TransactionStorageProof>(&Self::INHERENT_IDENTIFIER)
				.unwrap_or(None);
			proof.map(|proof| Call::check_proof { proof })
		}

		fn check_inherent(_call: &Self::Call, _data: &InherentData) -> Result<(), Self::Error> {
			Ok(())
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::check_proof { .. })
		}
	}

	impl<T: Config> Pallet<T> {
		/// Get transaction storage information from outside of this pallet.
		pub fn transaction_roots(
			block: BlockNumberFor<T>,
		) -> Option<BoundedVec<TransactionInfo, T::MaxBlockTransactions>> {
			Transactions::<T>::get(block)
		}

		pub fn indexed_transactions(
			block: BlockNumberFor<T>,
		) -> alloc::vec::Vec<sp_transaction_storage_proof::IndexedTransactionInfo> {
			const RAW_CID_CODEC: sp_transaction_storage_proof::CidCodec = 0x55;

			Transactions::<T>::get(block)
				.map(|txs| {
					txs.into_iter()
						.map(|tx| sp_transaction_storage_proof::IndexedTransactionInfo {
							content_hash: tx.content_hash.into(),
							size: tx.size,
							hashing: sp_transaction_storage_proof::HashingAlgorithm::Blake2b256,
							cid_codec: RAW_CID_CODEC,
							extrinsic_index: u32::MAX,
						})
						.collect()
				})
				.unwrap_or_default()
		}

		/// Get ByteFee storage information from the outside of this pallet.
		pub fn byte_fee() -> Option<BalanceOf<T>> {
			ByteFee::<T>::get()
		}
		/// Get EntryFee storage information from the outside of this pallet.
		pub fn entry_fee() -> Option<BalanceOf<T>> {
			EntryFee::<T>::get()
		}
		/// Get RetentionPeriod storage information from the outside of this pallet.
		pub fn retention_period() -> BlockNumberFor<T> {
			RetentionPeriod::<T>::get()
		}

		fn apply_fee(sender: T::AccountId, size: u32) -> DispatchResult {
			let byte_fee = ByteFee::<T>::get().ok_or(Error::<T>::NotConfigured)?;
			let entry_fee = EntryFee::<T>::get().ok_or(Error::<T>::NotConfigured)?;
			let fee = byte_fee.saturating_mul(size.into()).saturating_add(entry_fee);
			T::Currency::hold(&HoldReason::StorageFeeHold.into(), &sender, fee)?;
			let (credit, _remainder) =
				T::Currency::slash(&HoldReason::StorageFeeHold.into(), &sender, fee);
			debug_assert!(_remainder.is_zero());
			T::FeeDestination::on_unbalanced(credit);
			Ok(())
		}

		/// Returns `true` if a blob of the given size can be stored.
		fn data_size_ok(size: usize) -> bool {
			(size > 0) && (size <= T::MaxTransactionSize::get() as usize)
		}

		/// Ensures that the given data size is valid for storage.
		fn ensure_data_size_ok(size: usize) -> Result<(), Error<T>> {
			ensure!(Self::data_size_ok(size), Error::<T>::BadDataSize);
			Ok(())
		}

		/// Returns the [`TransactionInfo`] for the specified store/renew transaction.
		fn transaction_info(
			block_number: BlockNumberFor<T>,
			index: u32,
		) -> Option<TransactionInfo> {
			let transactions = Transactions::<T>::get(block_number)?;
			transactions.into_iter().nth(index as usize)
		}

		/// Verifies that the provided proof corresponds to a randomly selected chunk from a list of
		/// transactions.
		pub(crate) fn verify_chunk_proof(
			proof: TransactionStorageProof,
			random_hash: &[u8],
			infos: Vec<TransactionInfo>,
		) -> Result<(), Error<T>> {
			// Get the random chunk index - from all transactions in the block = [0..total_chunks).
			let total_chunks: ChunkIndex = TransactionInfo::total_chunks(&infos);
			ensure!(total_chunks != 0, Error::<T>::UnexpectedProof);
			let selected_block_chunk_index = random_chunk(random_hash, total_chunks);

			// Let's find the corresponding transaction and its "local" chunk index for "global"
			// `selected_block_chunk_index`.
			let (tx_info, tx_chunk_index) = {
				// Binary search for the transaction that owns this `selected_block_chunk_index`
				// chunk.
				let tx_index = infos
					.binary_search_by_key(&selected_block_chunk_index, |info| {
						// Each `info.block_chunks` is cumulative count,
						// so last chunk index = count - 1.
						info.block_chunks.saturating_sub(1)
					})
					.unwrap_or_else(|tx_index| tx_index);

				// Get the transaction and its local chunk index.
				let tx_info = infos.get(tx_index).ok_or(Error::<T>::MissingStateData)?;
				// We shouldn't reach this point; we rely on the fact that `fn store` does not allow
				// empty transactions. Without this check, it would fail anyway below with
				// `InvalidProof`.
				ensure!(!tx_info.block_chunks.is_zero(), Error::<T>::BadDataSize);

				// Convert a global chunk index into a transaction-local one.
				let tx_chunks = num_chunks(tx_info.size);
				let prev_chunks = tx_info.block_chunks - tx_chunks;
				let tx_chunk_index = selected_block_chunk_index - prev_chunks;

				(tx_info, tx_chunk_index)
			};

			// Verify the tx chunk proof.
			ensure!(
				sp_io::trie::blake2_256_verify_proof(
					tx_info.chunk_root,
					&proof.proof,
					&encode_index(tx_chunk_index),
					&proof.chunk,
					sp_runtime::StateVersion::V1,
				),
				Error::<T>::InvalidProof
			);

			Ok(())
		}
	}
}
