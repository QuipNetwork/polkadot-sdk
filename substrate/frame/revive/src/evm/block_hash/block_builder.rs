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

//! Ethereum block builder.

use crate::{
	Config, Pallet, ReceiptGasInfo,
	evm::{
		Block, HashesOrTransactionInfos, TYPE_EIP1559, TYPE_EIP2930, TYPE_EIP4844, TYPE_EIP7702,
		block_hash::{
			AccumulateReceipt, BuilderPhase, IncrementalHashBuilder, IncrementalHashBuilderIR,
			LogsBloom, receipt::BLOOM_SIZE_BYTES,
		},
	},
};

use alloc::{vec, vec::Vec};

use codec::{Decode, Encode};
use frame_support::traits::Time;
use scale_info::TypeInfo;
use sp_arithmetic::traits::Saturating;
use sp_core::{H160, H256, U256};
use sp_crypto_hashing::keccak_256;
use sp_runtime::traits::{One, Zero};

const LOG_TARGET: &str = "runtime::revive::block_builder";

/// Ethereum block builder designed to incrementally build the transaction and receipt trie roots.
///
/// This builder is optimized to minimize memory usage and pallet storage by leveraging the internal
/// structure of the Ethereum trie and the RLP encoding of receipts.
#[cfg_attr(test, derive(frame_support::DefaultNoBound))]
pub struct EthereumBlockBuilder<T> {
	pub(crate) transaction_root_builder: IncrementalHashBuilder,
	pub(crate) receipts_root_builder: IncrementalHashBuilder,
	pub(crate) tx_hashes: Vec<H256>,
	gas_used: U256,
	base_fee_per_gas: U256,
	block_gas_limit: U256,
	logs_bloom: LogsBloom,
	gas_info: Vec<ReceiptGasInfo>,
	_phantom: core::marker::PhantomData<T>,
}

impl<T: crate::Config> EthereumBlockBuilder<T> {
	/// Converts the builder into an intermediate representation.
	///
	/// The intermediate representation is extracted from the pallet storage.
	pub fn to_ir(self) -> EthereumBlockBuilderIR<T> {
		EthereumBlockBuilderIR {
			transaction_root_builder: self.transaction_root_builder.to_ir(),
			receipts_root_builder: self.receipts_root_builder.to_ir(),
			gas_used: self.gas_used,
			tx_hashes: self.tx_hashes,
			logs_bloom: self.logs_bloom.bloom,
			gas_info: self.gas_info,
			base_fee_per_gas: self.base_fee_per_gas,
			block_gas_limit: self.block_gas_limit,
			_phantom: core::marker::PhantomData,
		}
	}

	/// Converts the intermediate representation back into a builder.
	///
	/// The intermediate representation is placed into the pallet storage.
	pub fn from_ir(ir: EthereumBlockBuilderIR<T>) -> Self {
		Self {
			transaction_root_builder: IncrementalHashBuilder::from_ir(ir.transaction_root_builder),
			receipts_root_builder: IncrementalHashBuilder::from_ir(ir.receipts_root_builder),
			gas_used: ir.gas_used,
			base_fee_per_gas: ir.base_fee_per_gas,
			block_gas_limit: ir.block_gas_limit,
			tx_hashes: ir.tx_hashes,
			logs_bloom: LogsBloom { bloom: ir.logs_bloom },
			gas_info: ir.gas_info,
			_phantom: core::marker::PhantomData,
		}
	}

	/// Store the first transaction and receipt in pallet storage.
	fn pallet_put_first_values(&mut self, values: (Vec<u8>, Vec<u8>)) {
		crate::EthBlockBuilderFirstValues::<T>::put(Some(values));
	}

	/// Take the first transaction and receipt from pallet storage.
	fn pallet_take_first_values(&mut self) -> Option<(Vec<u8>, Vec<u8>)> {
		crate::EthBlockBuilderFirstValues::<T>::take()
	}

	/// Process a single transaction at a time.
	pub fn process_transaction(
		&mut self,
		transaction_encoded: Vec<u8>,
		success: bool,
		receipt_gas_info: ReceiptGasInfo,
		encoded_logs: Vec<u8>,
		receipt_bloom: LogsBloom,
	) {
		let tx_hash = H256(keccak_256(&transaction_encoded));
		self.tx_hashes.push(tx_hash);

		// Update the transaction trie.
		let transaction_type = Self::extract_transaction_type(transaction_encoded.as_slice());

		// Update gas and logs bloom.
		self.gas_used = self.gas_used.saturating_add(receipt_gas_info.gas_used);
		self.logs_bloom.accrue_bloom(&receipt_bloom);

		// Update the receipt trie.
		let encoded_receipt = AccumulateReceipt::encoded_receipt(
			encoded_logs,
			receipt_bloom,
			success,
			self.gas_used.as_u64(),
			transaction_type,
		);

		self.gas_info.push(receipt_gas_info);

		// The first transaction and receipt are returned to be stored in the pallet storage.
		// The index of the incremental hash builders already expects the next items.
		if self.tx_hashes.len() == 1 {
			log::trace!(target: LOG_TARGET, "Storing first transaction and receipt in pallet storage");
			self.pallet_put_first_values((transaction_encoded, encoded_receipt));
			return;
		}

		if self.transaction_root_builder.needs_first_value(BuilderPhase::ProcessingValue) {
			if let Some((first_tx, first_receipt)) = self.pallet_take_first_values() {
				log::trace!(target: LOG_TARGET, "Loaded first transaction and receipt from pallet storage");
				self.transaction_root_builder.set_first_value(first_tx);
				self.receipts_root_builder.set_first_value(first_receipt);
			} else {
				log::error!(target: LOG_TARGET, "First transaction and receipt must be present at processing phase");
			}
		}

		self.transaction_root_builder.add_value(transaction_encoded);
		self.receipts_root_builder.add_value(encoded_receipt);
	}

	/// Build the ethereum block from provided data.
	pub fn build_block(
		&mut self,
		block_number: crate::BlockNumberFor<T>,
	) -> (Block, Vec<ReceiptGasInfo>) {
		let parent_hash = if !Zero::is_zero(&block_number) {
			let prev_block_num = block_number.saturating_sub(One::one());
			crate::BlockHash::<T>::get(prev_block_num)
		} else {
			H256::default()
		};
		// Eth uses timestamps in seconds
		let timestamp = (T::Time::now() / 1000u32.into()).into();
		let block_author = Pallet::<T>::block_author();

		let eth_block_num: U256 = block_number.into();
		self.build_block_with_params(
			eth_block_num,
			self.base_fee_per_gas,
			parent_hash,
			timestamp,
			block_author,
			self.block_gas_limit,
		)
	}

	/// Build the ethereum block from provided parameters.
	/// This is useful for testing with custom block metadata.
	fn build_block_with_params(
		&mut self,
		block_number: U256,
		base_fee_per_gas: U256,
		parent_hash: H256,
		timestamp: U256,
		block_author: H160,
		block_gas_limit: U256,
	) -> (Block, Vec<ReceiptGasInfo>) {
		if self.transaction_root_builder.needs_first_value(BuilderPhase::Build) {
			if let Some((first_tx, first_receipt)) = self.pallet_take_first_values() {
				self.transaction_root_builder.set_first_value(first_tx);
				self.receipts_root_builder.set_first_value(first_receipt);
			} else {
				log::trace!(target: LOG_TARGET, "Building an empty block");
			}
		}

		let transactions_root = self.transaction_root_builder.finish();
		let receipts_root = self.receipts_root_builder.finish();

		let tx_hashes = core::mem::replace(&mut self.tx_hashes, Vec::new());
		let gas_info = core::mem::replace(&mut self.gas_info, Vec::new());

		let difficulty = U256::from(crate::vm::evm::DIFFICULTY);
		let mix_hash = H256(difficulty.to_big_endian());

		let mut block = Block {
			number: block_number,
			parent_hash,
			timestamp,
			miner: block_author,

			state_root: transactions_root,
			transactions_root,
			receipts_root,

			gas_limit: block_gas_limit,
			base_fee_per_gas,
			gas_used: self.gas_used,

			logs_bloom: self.logs_bloom.bloom.into(),
			transactions: HashesOrTransactionInfos::Hashes(tx_hashes),

			mix_hash,

			..Default::default()
		};

		let block_hash = block.header_hash();
		block.hash = block_hash;

		(block, gas_info)
	}

	/// Extracts the transaction type from the RLP encoded transaction.
	///
	/// This is needed to build the RLP encoding of the receipt.
	fn extract_transaction_type(transaction_encoded: &[u8]) -> Vec<u8> {
		// The transaction type is the first byte from the encoded transaction,
		// when the transaction is not legacy. For legacy transactions, there's
		// no type defined. Additionally, the RLP encoding of the tx type byte
		// is identical to the tx type.
		transaction_encoded
			.first()
			.cloned()
			.map(|first| match first {
				TYPE_EIP2930 | TYPE_EIP1559 | TYPE_EIP4844 | TYPE_EIP7702 => vec![first],
				_ => vec![],
			})
			.unwrap_or_default()
	}
}

/// The intermediate representation of the Ethereum block builder.
///
/// # Note
///
/// `base_fee_per_gas` and `block_gas_limit` are derived from the `NextFeeMultiplier`.
/// We store these values instead of computing them in `on_finalize` to ensure
/// they reflect the current block’s values, not those of the next block.
#[derive(Encode, Decode, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct EthereumBlockBuilderIR<T: Config> {
	transaction_root_builder: IncrementalHashBuilderIR,
	receipts_root_builder: IncrementalHashBuilderIR,

	base_fee_per_gas: U256,
	block_gas_limit: U256,
	gas_used: U256,
	logs_bloom: [u8; BLOOM_SIZE_BYTES],
	pub(crate) tx_hashes: Vec<H256>,
	pub(crate) gas_info: Vec<ReceiptGasInfo>,
	_phantom: core::marker::PhantomData<T>,
}

impl<T: Config> Default for EthereumBlockBuilderIR<T> {
	fn default() -> Self {
		Self {
			// Default not implemented for [u8; BLOOM_SIZE_BYTES]
			logs_bloom: [0; BLOOM_SIZE_BYTES],
			transaction_root_builder: IncrementalHashBuilderIR::default(),
			receipts_root_builder: IncrementalHashBuilderIR::default(),
			gas_used: U256::zero(),
			tx_hashes: Vec::new(),
			gas_info: Vec::new(),
			base_fee_per_gas: Pallet::<T>::evm_base_fee(),
			block_gas_limit: Pallet::<T>::evm_block_gas_limit(),
			_phantom: core::marker::PhantomData,
		}
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::{
		evm::Block,
		tests::{ExtBuilder, Test},
	};
	use alloy_core::rlp;
	use alloy_trie::{HashBuilder, Nibbles};
	use ethereum_types::Address;
	use pallet_revive_types::common::Bytes;
	use serde::Deserialize;

	/// Manual implementation of the Ethereum trie root computation.
	///
	/// Given the RLP encoded values, the implementation adjusts the
	/// index to account for RLP encoding rules.
	fn manual_trie_root_compute(encoded: Vec<Vec<u8>>) -> H256 {
		const fn adjust_index_for_rlp(i: usize, len: usize) -> usize {
			if i > 0x7f {
				i
			} else if i == 0x7f || i + 1 == len {
				0
			} else {
				i + 1
			}
		}

		let mut hb = HashBuilder::default();

		let items_len = encoded.len();
		for i in 0..items_len {
			let index = adjust_index_for_rlp(i, items_len);

			let index_buffer = rlp::encode_fixed_size(&index);
			hb.add_leaf(Nibbles::unpack(&index_buffer), &encoded[index]);

			// Each mask in these vectors holds a u16.
			let masks_len = (hb.state_masks.len() + hb.tree_masks.len() + hb.hash_masks.len()) * 2;
			let _size = hb.key.len() +
				hb.value.as_slice().len() +
				hb.stack.len() * 33 +
				masks_len + hb.rlp_buf.len();
		}

		hb.root().0.into()
	}

	/// The test compares three hashing options:
	/// - Block::compute_trie_root: this uses the consensus proofs crate
	/// - manual_trie_root_compute: this ensures the keys are added in the correct order
	/// - IncrementalHashBuilder: this offers the most compact storage option
	///
	/// The above hashes must be identical. While at it, the incremental hash
	/// builder is serialized and deserialized to ensure consistency.
	#[test]
	fn incremental_hasher() {
		const UPPER_BOUND: usize = 256;
		const RLP_VALUE_SIZE: usize = 128;

		let mut rlp_values = Vec::with_capacity(UPPER_BOUND);

		for i in 0..UPPER_BOUND {
			// Simulate an RLP value repeated for `i`.
			let rlp_value = vec![i as u8; RLP_VALUE_SIZE];

			rlp_values.push(rlp_value);

			let block_hash: H256 = Block::compute_trie_root(&rlp_values).0.into();
			let manual_hash = manual_trie_root_compute(rlp_values.clone());

			let mut first_value = Some(rlp_values[0].clone());
			let mut builder = IncrementalHashBuilder::default();
			for rlp_value in rlp_values.iter().skip(1) {
				if builder.needs_first_value(BuilderPhase::ProcessingValue) {
					let value = first_value.take().expect("First value must be present; qed");
					builder.set_first_value(value);
				}
				builder.add_value(rlp_value.clone());

				let ir_builder = builder.to_ir();
				builder = IncrementalHashBuilder::from_ir(ir_builder);
			}
			if let Some(value) = first_value.take() {
				builder.set_first_value(value);
			}
			let incremental_hash = builder.finish();

			assert_eq!(block_hash, manual_hash);
			assert_eq!(block_hash, incremental_hash);
		}
	}

	#[test]
	fn test_alloy_rlp_ordering_compatibility() {
		let zero_encoded = rlp::encode_fixed_size(&0usize);
		let max_single_byte = rlp::encode_fixed_size(&127usize);
		let first_multi_byte = rlp::encode_fixed_size(&128usize);

		// Document the exact bytes we expect
		assert_eq!(zero_encoded.as_slice(), &[0x80]); // RLP encoding of 0
		assert_eq!(max_single_byte.as_slice(), &[0x7f]); // RLP encoding of 127
		assert_eq!(first_multi_byte.as_slice(), &[0x81, 0x80]); // RLP encoding of 128

		// Verify ordering
		assert!(max_single_byte < zero_encoded);
		assert!(zero_encoded < first_multi_byte);
	}

	#[test]
	fn ensure_identical_hashes() {
		#[derive(Deserialize)]
		#[serde(rename_all = "camelCase")]
		struct ReceiptInfo {
			block_hash: H256,
			status: Option<U256>,
			effective_gas_price: U256,
			gas_used: U256,
			transaction_index: U256,
			logs: Vec<Log>,
		}

		#[derive(Deserialize)]
		pub struct Log {
			address: Address,
			data: Option<Bytes>,
			topics: Vec<H256>,
		}

		// Test data files collected with ./test-assets/get_test_data.sh
		let test_data = [
			(
				"./test-assets/block_0x161bd0f_ethereum-mainnet.json",
				"./test-assets/receipts_0x161bd0f_ethereum-mainnet.json",
			),
			(
				"./test-assets/block_0x151241d_ethereum-mainnet.json",
				"./test-assets/receipts_0x151241d_ethereum-mainnet.json",
			),
			(
				"./test-assets/block_0x874db3_ethereum-sepolia.json",
				"./test-assets/receipts_0x874db3_ethereum-sepolia.json",
			),
		];

		for (block_path, receipts_path) in test_data {
			let json = std::fs::read_to_string(block_path).unwrap();
			let block: pallet_revive_types::runtime_api::BlockV1 =
				serde_json::from_str(&json).unwrap();
			let block: Block = codec::Decode::decode(&mut &codec::Encode::encode(&block)[..])
				.expect("BlockV1 and Block must be scale-compatible");

			let json = std::fs::read_to_string(receipts_path).unwrap();
			let receipts: Vec<ReceiptInfo> = serde_json::from_str(&json).unwrap();

			assert_eq!(block.header_hash(), receipts[0].block_hash);

			let tx = match &block.transactions {
				HashesOrTransactionInfos::TransactionInfos(infos) => infos.clone(),
				_ => panic!("Expected full tx body"),
			};

			let encoded_tx: Vec<_> = tx
				.clone()
				.into_iter()
				.map(|tx| tx.transaction_signed.signed_payload())
				.collect();

			let transaction_details: Vec<_> = tx
				.into_iter()
				.zip(receipts.into_iter())
				.map(|(tx_info, receipt_info)| {
					if tx_info.transaction_index != receipt_info.transaction_index {
						panic!("Transaction and receipt index do not match");
					}

					let logs: Vec<_> = receipt_info
						.logs
						.into_iter()
						.map(|log| (log.address, log.data.unwrap_or_default().0, log.topics))
						.collect();

					(
						tx_info.transaction_signed.signed_payload(),
						logs,
						receipt_info.status.unwrap_or_default() == 1.into(),
						receipt_info.gas_used,
						receipt_info.effective_gas_price,
					)
				})
				.collect();

			ExtBuilder::default().build().execute_with(|| {
				// Build the ethereum block incrementally.
				let mut incremental_block = EthereumBlockBuilder::<Test>::default();
				for (signed, logs, success, gas_used, effective_gas_price) in transaction_details {
					let mut log_size = 0;

					let mut accumulate_receipt = AccumulateReceipt::new();
					for (address, data, topics) in &logs {
						let current_size = data.len() + topics.len() * 32 + 20;
						log_size += current_size;
						accumulate_receipt.add_log(address, data, topics);
					}

					incremental_block.process_transaction(
						signed,
						success,
						ReceiptGasInfo { gas_used, effective_gas_price },
						accumulate_receipt.encoding,
						accumulate_receipt.bloom,
					);

					let ir = incremental_block.to_ir();
					incremental_block = EthereumBlockBuilder::from_ir(ir);
					log::trace!(target: LOG_TARGET, " Log size {:?}", log_size);
				}

				// The block hash would differ here because we don't take into account
				// the ommers and other fields from the substrate perspective.
				// However, the state roots must be identical.
				let built_block = incremental_block
					.build_block_with_params(
						block.number,
						block.base_fee_per_gas,
						block.parent_hash,
						block.timestamp,
						block.miner,
						block.gas_limit,
					)
					.0;

				assert_eq!(built_block.gas_used, block.gas_used);
				assert_eq!(built_block.logs_bloom, block.logs_bloom);
				// We are using the tx root for state root.
				assert_eq!(built_block.state_root, built_block.transactions_root);

				// Double check the receipts roots.
				assert_eq!(built_block.receipts_root, block.receipts_root);

				let manual_hash = manual_trie_root_compute(encoded_tx.clone());

				let mut total_size = 0;
				for enc in &encoded_tx {
					total_size += enc.len();
				}
				log::trace!(target: LOG_TARGET, "Total size used by transactions: {:?}", total_size);

				let mut builder = IncrementalHashBuilder::default();
				let mut loaded = false;
				for tx in encoded_tx.iter().skip(1) {
					if builder.needs_first_value(BuilderPhase::ProcessingValue) {
						loaded = true;
						let first_tx = encoded_tx[0].clone();
						builder.set_first_value(first_tx);
					}
					builder.add_value(tx.clone())
				}
				if !loaded {
					// Not loaded, therefore the first value must be set now.
					assert!(builder.needs_first_value(BuilderPhase::Build));

					let first_tx = encoded_tx[0].clone();
					builder.set_first_value(first_tx);
				}

				let incremental_hash = builder.finish();

				log::trace!(target: LOG_TARGET, "Incremental hash: {:?}", incremental_hash);
				log::trace!(target: LOG_TARGET, "Manual Hash: {:?}", manual_hash);
				log::trace!(target: LOG_TARGET, "Built block Hash: {:?}", built_block.transactions_root);
				log::trace!(target: LOG_TARGET, "Real Block Tx Hash: {:?}", block.transactions_root);

				assert_eq!(incremental_hash, block.transactions_root);

				// This double checks the compute logic.
				assert_eq!(manual_hash, block.transactions_root);
				// This ensures we can compute the same transaction root as Ethereum.
				assert_eq!(block.transactions_root, built_block.transactions_root);
			});
		}
	}
}
