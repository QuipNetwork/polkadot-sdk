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

//! Handle encoded RLP ethereum receipts for block hash computation.

use alloc::vec::Vec;
use alloy_core::rlp;
use sp_core::{H160, H256};
use sp_crypto_hashing::keccak_256;

/// Number of bytes that a bloom stores.
pub const BLOOM_SIZE_BYTES: usize = 256;

/// Accumulate events (logs) into a stream of RLP encoded bytes.
/// This is a straight forward implementation that RLP encodes logs as they are added.
///
/// The main goal is to generate the RLP-encoded representation of receipts
/// which is required to compute the receipt root hash, without storing the full receipt
/// data in memory.
///
/// One approach is to store the full receipt in memory, together with the RLP encoding
/// of the receipt.
///
/// However, since we only care about the RLP encoding of the receipt, we can optimize the memory
/// usage by only storing the RLP encoded value and the logs directly. This effectively saves
/// the need to store the full receipt (which can grow unboundedly due to the number of logs), and
/// builds the RLP encoding incrementally as logs are added.
///
/// The implementation leverages the RLP encoding details of the receipt:
///
/// ```ignore
/// // Memory representation of the RLP encoded receipt:
/// [
/// 	ReceiptHeader ++ rlp(status) ++ rlp(gas) ++ rlp(bloom)
/// 			++ LogsHeader ++ rlp(log1) ++ rlp(log2) ++ ... ++ rlp(logN)
/// ]
/// ```
///
/// The optimization comes from the fact that `rlp(log1) ++ rlp(log2) ++ ... ++ rlp(logN)`
/// can be built incrementally.
///
/// On average, from the real ethereum block, this implementation reduces the memory usage by 30%.
///  `EncodedReceipt Space optimization (on average): 0.6995642434146292`
pub struct AccumulateReceipt {
	/// The RLP bytes where the logs are accumulated.
	pub encoding: Vec<u8>,
	/// The bloom filter collected from accumulating logs.
	pub bloom: LogsBloom,
}

impl AccumulateReceipt {
	/// Constructs a new [`AccumulateReceipt`].
	pub const fn new() -> Self {
		Self { encoding: Vec::new(), bloom: LogsBloom::new() }
	}

	/// Add the log into the accumulated receipt.
	///
	/// This accrues the log bloom and keeps track of the RLP encoding of the log.
	pub fn add_log(&mut self, contract: &H160, data: &[u8], topics: &[H256]) {
		// Accrue the log bloom.
		self.bloom.accrue_log(contract, topics);

		// Determine the length of the log RLP encoding.
		let mut topics_len: usize = 0;
		for topic in topics {
			// Topics are represented by 32 bytes. However, their encoding
			// can produce different lengths depending on their value.
			topics_len = topics_len.saturating_add(rlp::Encodable::length(&topic.0));
		}
		// Account for the size of the list header.
		let topics_list_header_length = topics_len + rlp::length_of_length(topics_len);
		// Compute the total payload length of the log.
		let payload_length = rlp::Encodable::length(&contract.0) +
			rlp::Encodable::length(&data) +
			topics_list_header_length;

		let header = rlp::Header { list: true, payload_length };
		header.encode(&mut self.encoding);
		rlp::Encodable::encode(&contract.0, &mut self.encoding);
		// Encode the topics as a list
		rlp::Header { list: true, payload_length: topics_len }.encode(&mut self.encoding);
		for topic in topics {
			rlp::Encodable::encode(&topic.0, &mut self.encoding);
		}
		rlp::Encodable::encode(&data, &mut self.encoding);
	}

	/// Finalize the accumulated receipt and return the RLP encoded bytes.
	pub fn encoded_receipt(
		encoded_logs: Vec<u8>,
		bloom: LogsBloom,
		status: bool,
		gas: u64,
		transaction_type: Vec<u8>,
	) -> Vec<u8> {
		let logs_length = encoded_logs.len();
		let list_header_length = logs_length + rlp::length_of_length(logs_length);

		let header = rlp::Header {
			list: true,
			payload_length: rlp::Encodable::length(&status) +
				rlp::Encodable::length(&gas) +
				rlp::Encodable::length(&bloom.bloom) +
				list_header_length,
		};

		let mut encoded = transaction_type;
		header.encode(&mut encoded);
		rlp::Encodable::encode(&status, &mut encoded);
		rlp::Encodable::encode(&gas, &mut encoded);
		rlp::Encodable::encode(&bloom.bloom, &mut encoded);

		let logs_header = rlp::Header { list: true, payload_length: logs_length };
		logs_header.encode(&mut encoded);

		encoded.extend(encoded_logs);

		encoded
	}
}

/// Bloom log filter compatible with Ethereum implementation.
///
/// This structure avoids conversions between substrate to alloy types
/// to optimally compute the bloom.
#[derive(Clone, Copy)]
pub struct LogsBloom {
	/// The bloom bytes used to store logs.
	pub bloom: [u8; BLOOM_SIZE_BYTES],
}

impl Default for LogsBloom {
	fn default() -> Self {
		Self::new()
	}
}

impl LogsBloom {
	/// Constructs a new [`LogsBloom`].
	pub const fn new() -> Self {
		Self { bloom: [0u8; BLOOM_SIZE_BYTES] }
	}

	/// Ingests a raw log (event) into the bloom filter.
	pub fn accrue_log(&mut self, contract: &H160, topics: &[H256]) {
		Self::m3_2048(&mut self.bloom, contract.as_ref());

		for topic in topics {
			Self::m3_2048(&mut self.bloom, topic.as_ref());
		}
	}

	/// Accrues the input into the bloom filter.
	pub fn accrue_bloom(&mut self, other: &Self) {
		for i in 0..BLOOM_SIZE_BYTES {
			self.bloom[i] |= other.bloom[i];
		}
	}

	/// Specialized Bloom filter that sets three bits out of 2048, given an
	/// arbitrary byte sequence.
	///
	/// See Section 4.3.1 "Transaction Receipt" of the
	/// [Ethereum Yellow Paper][ref] (page 6).
	///
	/// [ref]: https://ethereum.github.io/yellowpaper/paper.pdf
	fn m3_2048(bloom: &mut [u8; 256], bytes: &[u8]) {
		let hash = keccak_256(bytes);
		for i in [0, 2, 4] {
			let bit = (hash[i + 1] as usize + ((hash[i] as usize) << 8)) & 0x7FF;
			bloom[256 - 1 - bit / 8] |= 1 << (bit % 8);
		}
	}
}

#[cfg(test)]
mod test {
	use super::*;
	use alloy_consensus::RlpEncodableReceipt;

	#[test]
	fn test_bloom_accrue_log() {
		let mut bloom = LogsBloom::new();
		let data = vec![
			(H160::repeat_byte(0x01), vec![H256::repeat_byte(0x02), H256::repeat_byte(0x03)]),
			(H160::repeat_byte(0x04), vec![H256::repeat_byte(0x05), H256::repeat_byte(0x06)]),
			(H160::repeat_byte(0x07), vec![H256::repeat_byte(0x08), H256::repeat_byte(0x09)]),
		];

		for (contract, topics) in data.clone() {
			bloom.accrue_log(&contract, &topics);
		}

		let mut alloy_bloom = alloy_core::primitives::Bloom::default();

		for (contract, topics) in data {
			alloy_bloom.accrue_raw_log(
				contract.0.into(),
				&topics.iter().map(|t| t.0.into()).collect::<Vec<_>>(),
			);
		}

		assert_eq!(bloom.bloom, alloy_bloom.0);
	}

	#[test]
	fn test_bloom_accrue_bloom() {
		let mut bloom = LogsBloom::new();
		let mut bloom2 = LogsBloom::new();

		bloom.accrue_log(&H160::repeat_byte(0x01), &[H256::repeat_byte(0x02)]);
		bloom2.accrue_log(&H160::repeat_byte(0x03), &[H256::repeat_byte(0x04)]);
		bloom.accrue_bloom(&bloom2);

		let mut alloy_bloom = alloy_core::primitives::Bloom::default();
		let mut alloy_bloom2 = alloy_core::primitives::Bloom::default();
		alloy_bloom
			.accrue_raw_log(H160::repeat_byte(0x01).0.into(), &[H256::repeat_byte(0x02).0.into()]);
		alloy_bloom2
			.accrue_raw_log(H160::repeat_byte(0x03).0.into(), &[H256::repeat_byte(0x04).0.into()]);
		alloy_bloom.accrue_bloom(&alloy_bloom2);

		assert_eq!(bloom.bloom, alloy_bloom.0);
	}

	#[test]
	fn test_accumulate_receipt() {
		let mut receipt = AccumulateReceipt::new();

		receipt.add_log(&H160::repeat_byte(0x01), &[0x01, 0x02], &[H256::repeat_byte(0x02)]);
		receipt.add_log(&H160::repeat_byte(0x03), &[0x03, 0x04], &[H256::repeat_byte(0x04)]);

		let encoded = AccumulateReceipt::encoded_receipt(
			receipt.encoding,
			receipt.bloom,
			true,
			21000,
			vec![],
		);

		let alloy_receipt = alloy_consensus::Receipt {
			status: true.into(),
			cumulative_gas_used: 21000,
			logs: vec![
				alloy_core::primitives::Log::new_unchecked(
					H160::repeat_byte(0x01).0.into(),
					vec![H256::repeat_byte(0x02).0.into()],
					vec![0x01, 0x02].into(),
				),
				alloy_core::primitives::Log::new_unchecked(
					H160::repeat_byte(0x03).0.into(),
					vec![H256::repeat_byte(0x04).0.into()],
					vec![0x03, 0x04].into(),
				),
			],
		};

		// Check bloom filters.
		let alloy_bloom = alloy_receipt.bloom_slow();
		assert_eq!(receipt.bloom.bloom, alloy_bloom.0);

		// Check RLP encoding.
		let mut alloy_encoded = vec![];
		alloy_receipt.rlp_encode_with_bloom(&alloy_bloom, &mut alloy_encoded);

		assert_eq!(alloy_encoded, encoded);
	}
}
