// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::*;
use serde::{Deserialize, Serialize};

/// Receipt information
#[derive(Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptInfo {
	/// blob gas price
	/// The actual value per gas deducted from the sender's account for blob gas. Only specified
	/// for blob transactions as defined by EIP-4844.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub blob_gas_price: Option<U256>,
	/// blob gas used
	/// The amount of blob gas used for this specific transaction. Only specified for blob
	/// transactions as defined by EIP-4844.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub blob_gas_used: Option<U256>,
	/// block hash
	pub block_hash: H256,
	/// block number
	pub block_number: U256,
	/// contract address
	/// The contract address created, if the transaction was a contract creation, otherwise null.
	pub contract_address: Option<Address>,
	/// cumulative gas used
	/// The sum of gas used by this transaction and all preceding transactions in the same block.
	pub cumulative_gas_used: U256,
	/// effective gas price
	/// The actual value per gas deducted from the sender's account. Before EIP-1559, this is equal
	/// to the transaction's gas price. After, it is equal to baseFeePerGas + min(maxFeePerGas -
	/// baseFeePerGas, maxPriorityFeePerGas).
	pub effective_gas_price: U256,
	/// from
	pub from: Address,
	/// gas used
	/// The amount of gas used for this specific transaction alone.
	pub gas_used: U256,
	/// logs
	pub logs: Vec<Log>,
	/// logs bloom
	pub logs_bloom: Bytes256,
	/// state root
	/// The post-transaction state root. Only specified for transactions included before the
	/// Byzantium upgrade.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub root: Option<H256>,
	/// status
	/// Either 1 (success) or 0 (failure). Only specified for transactions included after the
	/// Byzantium upgrade.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub status: Option<U256>,
	/// to
	/// Address of the receiver or null in a contract creation transaction.
	pub to: Option<Address>,
	/// transaction hash
	pub transaction_hash: H256,
	/// transaction index
	pub transaction_index: U256,
	/// type
	#[serde(skip_serializing_if = "Option::is_none")]
	pub r#type: Option<Byte>,
}

impl ReceiptInfo {
	/// Initialize a new Receipt
	pub fn new(
		block_hash: H256,
		block_number: U256,
		contract_address: Option<Address>,
		from: Address,
		logs: Vec<Log>,
		to: Option<Address>,
		effective_gas_price: U256,
		gas_used: U256,
		success: bool,
		transaction_hash: H256,
		transaction_index: U256,
		r#type: Byte,
	) -> Self {
		let logs_bloom = Self::logs_bloom(&logs);
		ReceiptInfo {
			block_hash,
			block_number,
			contract_address,
			from,
			logs,
			logs_bloom,
			to,
			effective_gas_price,
			gas_used,
			status: Some(if success { U256::one() } else { U256::zero() }),
			transaction_hash,
			transaction_index,
			r#type: Some(r#type),
			..Default::default()
		}
	}

	/// Returns `true` if the transaction was successful.
	pub fn is_success(&self) -> bool {
		self.status.map_or(false, |status| status == U256::one())
	}

	/// Calculate receipt logs bloom.
	fn logs_bloom(logs: &[Log]) -> Bytes256 {
		let mut bloom = [0u8; 256];
		for log in logs {
			m3_2048(&mut bloom, &log.address.as_ref());
			for topic in &log.topics {
				m3_2048(&mut bloom, topic.as_ref());
			}
		}
		bloom.into()
	}

	/// Create a new [`TransactionInfoV1`] from a receipt and a signed transaction.
	pub fn transaction_info(&self, transaction_signed: TransactionSigned) -> TransactionInfoV1 {
		TransactionInfoV1 {
			block_hash: self.block_hash,
			block_number: self.block_number,
			from: self.from,
			hash: self.transaction_hash,
			transaction_index: self.transaction_index,
			transaction_signed: transaction_signed.into(),
		}
	}
}

/// Specialised Bloom filter that sets three bits out of 2048, given an
/// arbitrary byte sequence.
///
/// See Section 4.4.1 "Transaction Receipt" of the [Ethereum Yellow Paper][ref].
///
/// [ref]: https://ethereum.github.io/yellowpaper/paper.pdf
fn m3_2048(bloom: &mut [u8; 256], bytes: &[u8]) {
	let hash = keccak_256(bytes);
	for i in [0, 2, 4] {
		let bit = (hash[i + 1] as usize + ((hash[i] as usize) << 8)) & 0x7FF;
		bloom[256 - 1 - bit / 8] |= 1 << (bit % 8);
	}
}

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn logs_bloom_works() {
		let receipt: ReceiptInfo = serde_json::from_str(
		r#"
		{
			"blockHash": "0x835ee379aaabf4802a22a93ad8164c02bbdde2cc03d4552d5c642faf4e09d1f3",
			"blockNumber": "0x2",
			"contractAddress": null,
			"cumulativeGasUsed": "0x5d92",
			"effectiveGasPrice": "0x2dcd5c2d",
			"from": "0xb4f1f9ecfe5a28633a27f57300bda217e99b8969",
			"gasUsed": "0x5d92",
			"logs": [
				{
				"address": "0x82bdb002b9b1f36c42df15fbdc6886abcb2ab31d",
				"topics": [
					"0x1585375487296ff2f0370daeec4214074a032b31af827c12622fa9a58c16c7d0",
					"0x000000000000000000000000b4f1f9ecfe5a28633a27f57300bda217e99b8969"
				],
				"data": "0x00000000000000000000000000000000000000000000000000000000000030390000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000b48656c6c6f20776f726c64000000000000000000000000000000000000000000",
				"blockNumber": "0x2",
				"transactionHash": "0xad0075127962bdf73d787f2944bdb5f351876f23c35e6a48c1f5b6463a100af4",
				"transactionIndex": "0x0",
				"blockHash": "0x835ee379aaabf4802a22a93ad8164c02bbdde2cc03d4552d5c642faf4e09d1f3",
				"logIndex": "0x0",
				"removed": false
				}
			],
			"logsBloom": "0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000400000008000000000000000000000000000000000000000000000000800000000040000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000004000000000000000800000000000000000080000000000000000000000000000000000000000000",
			"status": "0x1",
			"to": "0x82bdb002b9b1f36c42df15fbdc6886abcb2ab31d",
			"transactionHash": "0xad0075127962bdf73d787f2944bdb5f351876f23c35e6a48c1f5b6463a100af4",
			"transactionIndex": "0x0",
			"type": "0x2"
		}
		"#,
	)
	.unwrap();
		assert_eq!(receipt.logs_bloom, ReceiptInfo::logs_bloom(&receipt.logs));
	}
}
