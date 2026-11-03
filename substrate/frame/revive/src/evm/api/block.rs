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

use super::{Bytes, Bytes8, Bytes256, HashesOrTransactionInfos};
use alloc::vec::Vec;
use codec::{Decode, Encode};
use ethereum_types::*;
use pallet_revive_types::runtime_api::*;
use scale_info::TypeInfo;

/// Block object
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode)]
#[allow(missing_docs)]
pub struct Block {
	/// Base fee per gas
	pub base_fee_per_gas: U256,
	/// Blob gas used
	pub blob_gas_used: U256,
	/// Difficulty
	pub difficulty: U256,
	/// Excess blob gas
	pub excess_blob_gas: U256,
	/// Extra data
	pub extra_data: Bytes,
	/// Gas limit
	pub gas_limit: U256,
	/// Gas used
	pub gas_used: U256,
	/// Hash
	pub hash: H256,
	/// Bloom filter
	pub logs_bloom: Bytes256,
	/// Coinbase
	pub miner: Address,
	/// Mix hash
	pub mix_hash: H256,
	/// Nonce
	pub nonce: Bytes8,
	/// Number
	pub number: U256,
	/// Parent Beacon Block Root
	pub parent_beacon_block_root: Option<H256>,
	/// Parent block hash
	pub parent_hash: H256,
	/// Receipts root
	pub receipts_root: H256,
	/// Requests root
	pub requests_hash: Option<H256>,
	/// Ommers hash
	pub sha_3_uncles: H256,
	/// Block size
	pub size: U256,
	/// State root
	pub state_root: H256,
	/// Timestamp
	pub timestamp: U256,
	/// Total difficulty
	pub total_difficulty: Option<U256>,
	pub transactions: HashesOrTransactionInfos,
	/// Transactions root
	pub transactions_root: H256,
	/// Uncles
	pub uncles: Vec<H256>,
	/// Withdrawals
	pub withdrawals: Vec<Withdrawal>,
	/// Withdrawals root
	pub withdrawals_root: H256,
}

impl From<Block> for BlockV1 {
	fn from(value: Block) -> Self {
		Self {
			base_fee_per_gas: value.base_fee_per_gas,
			blob_gas_used: value.blob_gas_used,
			difficulty: value.difficulty,
			excess_blob_gas: value.excess_blob_gas,
			extra_data: value.extra_data,
			gas_limit: value.gas_limit,
			gas_used: value.gas_used,
			hash: value.hash,
			logs_bloom: value.logs_bloom,
			miner: value.miner,
			mix_hash: value.mix_hash,
			nonce: value.nonce,
			number: value.number,
			parent_beacon_block_root: value.parent_beacon_block_root,
			parent_hash: value.parent_hash,
			receipts_root: value.receipts_root,
			requests_hash: value.requests_hash,
			sha_3_uncles: value.sha_3_uncles,
			size: value.size,
			state_root: value.state_root,
			timestamp: value.timestamp,
			total_difficulty: value.total_difficulty,
			transactions: value.transactions.into(),
			transactions_root: value.transactions_root,
			uncles: value.uncles,
			withdrawals: value.withdrawals.into_iter().map(Into::into).collect(),
			withdrawals_root: value.withdrawals_root,
		}
	}
}

/// Validator withdrawal
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode)]
pub struct Withdrawal {
	/// recipient address for withdrawal value
	pub address: Address,
	/// value contained in withdrawal
	pub amount: U256,
	/// index of withdrawal
	pub index: U256,
	/// index of validator that generated withdrawal
	pub validator_index: U256,
}

impl From<Withdrawal> for WithdrawalV1 {
	fn from(value: Withdrawal) -> Self {
		Self {
			address: value.address,
			amount: value.amount,
			index: value.index,
			validator_index: value.validator_index,
		}
	}
}
