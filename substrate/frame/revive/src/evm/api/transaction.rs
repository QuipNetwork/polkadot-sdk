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

#![allow(missing_docs)]

use super::{
	Byte, Bytes, TYPE_EIP1559, TYPE_EIP2930, TYPE_EIP4844, TYPE_EIP7702, TYPE_LEGACY, TypeEip1559,
	TypeEip2930, TypeEip4844, TypeEip7702, TypeLegacy,
};
use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode};
use derive_more::{From, TryInto};
use ethereum_types::*;
use pallet_revive_types::runtime_api::*;
use scale_info::TypeInfo;

/// Transaction object generic to all types
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct GenericTransaction {
	/// accessList
	/// EIP-2930 access list
	pub access_list: Option<AccessList>,
	/// authorizationList
	/// List of account code authorizations (EIP-7702)
	pub authorization_list: Vec<AuthorizationListEntry>,
	/// blobVersionedHashes
	/// List of versioned blob hashes associated with the transaction's EIP-4844 data blobs.
	pub blob_versioned_hashes: Vec<H256>,
	/// blobs
	/// Raw blob data.
	pub blobs: Vec<Bytes>,
	/// chainId
	/// Chain ID that this transaction is valid on.
	pub chain_id: Option<U256>,
	/// from address
	pub from: Option<Address>,
	/// gas limit
	pub gas: Option<U256>,
	/// gas price
	/// The gas price willing to be paid by the sender in wei
	pub gas_price: Option<U256>,
	/// input data
	pub input: InputOrData,
	/// max fee per blob gas
	/// The maximum total fee per gas the sender is willing to pay for blob gas in wei
	pub max_fee_per_blob_gas: Option<U256>,
	/// max fee per gas
	/// The maximum total fee per gas the sender is willing to pay (includes the network / base fee
	/// and miner / priority fee) in wei
	pub max_fee_per_gas: Option<U256>,
	/// max priority fee per gas
	/// Maximum fee per gas the sender is willing to pay to miners in wei
	pub max_priority_fee_per_gas: Option<U256>,
	/// nonce
	pub nonce: Option<U256>,
	/// to address
	pub to: Option<Address>,
	/// type
	pub r#type: Option<Byte>,
	/// value
	pub value: Option<U256>,
}

impl GenericTransaction {
	/// Create a new [`GenericTransaction`] from a signed transaction.
	pub fn from_signed(tx: TransactionSigned, base_gas_price: U256, from: Option<H160>) -> Self {
		Self::from_unsigned(tx.into(), base_gas_price, from)
	}

	/// Returns `true` when the transaction's payload fields look like those of a simple value
	/// transfer: empty calldata, no access list, no EIP-7702 authorization list, no EIP-4844 blob
	/// payload, and no blob gas fee. The destination address is validated separately by the caller.
	pub fn has_simple_transfer_fields(&self) -> bool {
		self.input.is_empty() &&
			self.access_list.as_ref().is_none_or(|list| list.is_empty()) &&
			self.authorization_list.is_empty() &&
			self.blob_versioned_hashes.is_empty() &&
			self.blobs.is_empty() &&
			self.max_fee_per_blob_gas.is_none()
	}

	/// The gas price that is actually paid (including priority fee).
	pub fn effective_gas_price(&self, base_gas_price: U256) -> Option<U256> {
		let effective_gas_price = if let Some(prio_price) = self.max_priority_fee_per_gas {
			let max_price = self.max_fee_per_gas?;
			Some(max_price.min(base_gas_price.saturating_add(prio_price)))
		} else {
			self.gas_price
		};

		// we do not implement priority fee as it does not map to tip well
		// hence the effective gas price cannot be higher than the base price
		effective_gas_price.map(|e| e.min(base_gas_price))
	}

	/// Create a new [`GenericTransaction`] from a unsigned transaction.
	pub fn from_unsigned(
		tx: TransactionUnsigned,
		base_gas_price: U256,
		from: Option<H160>,
	) -> Self {
		use TransactionUnsigned::*;
		let mut tx = match tx {
			TransactionLegacyUnsigned(tx) => GenericTransaction {
				from,
				r#type: Some(tx.r#type.as_byte()),
				chain_id: tx.chain_id,
				input: tx.input.into(),
				nonce: Some(tx.nonce),
				value: Some(tx.value),
				to: tx.to,
				gas: Some(tx.gas),
				gas_price: Some(tx.gas_price),
				..Default::default()
			},
			Transaction4844Unsigned(tx) => GenericTransaction {
				from,
				r#type: Some(tx.r#type.as_byte()),
				chain_id: Some(tx.chain_id),
				input: tx.input.into(),
				nonce: Some(tx.nonce),
				value: Some(tx.value),
				to: Some(tx.to),
				gas: Some(tx.gas),
				access_list: Some(tx.access_list),
				blob_versioned_hashes: tx.blob_versioned_hashes,
				max_fee_per_blob_gas: Some(tx.max_fee_per_blob_gas),
				max_fee_per_gas: Some(tx.max_fee_per_gas),
				max_priority_fee_per_gas: Some(tx.max_priority_fee_per_gas),
				..Default::default()
			},
			Transaction1559Unsigned(tx) => GenericTransaction {
				from,
				r#type: Some(tx.r#type.as_byte()),
				chain_id: Some(tx.chain_id),
				input: tx.input.into(),
				nonce: Some(tx.nonce),
				value: Some(tx.value),
				to: tx.to,
				gas: Some(tx.gas),
				access_list: Some(tx.access_list),
				max_fee_per_gas: Some(tx.max_fee_per_gas),
				max_priority_fee_per_gas: Some(tx.max_priority_fee_per_gas),
				..Default::default()
			},
			Transaction2930Unsigned(tx) => GenericTransaction {
				from,
				r#type: Some(tx.r#type.as_byte()),
				chain_id: Some(tx.chain_id),
				input: tx.input.into(),
				nonce: Some(tx.nonce),
				value: Some(tx.value),
				to: tx.to,
				gas: Some(tx.gas),
				gas_price: Some(tx.gas_price),
				access_list: Some(tx.access_list),
				..Default::default()
			},
			Transaction7702Unsigned(tx) => GenericTransaction {
				from,
				r#type: Some(tx.r#type.as_byte()),
				chain_id: Some(tx.chain_id),
				input: tx.input.into(),
				nonce: Some(tx.nonce),
				value: Some(tx.value),
				to: Some(tx.to),
				gas: Some(tx.gas),
				access_list: Some(tx.access_list),
				authorization_list: tx.authorization_list,
				max_fee_per_gas: Some(tx.max_fee_per_gas),
				max_priority_fee_per_gas: Some(tx.max_priority_fee_per_gas),
				..Default::default()
			},
		};
		tx.gas_price = tx.effective_gas_price(base_gas_price);
		tx
	}

	/// Convert to a [`TransactionUnsigned`].
	pub fn try_into_unsigned(self) -> Result<TransactionUnsigned, ()> {
		match self.r#type.unwrap_or_default().0 {
			TYPE_LEGACY => Ok(TransactionLegacyUnsigned {
				r#type: TypeLegacy {},
				chain_id: self.chain_id,
				input: self.input.to_bytes(),
				nonce: self.nonce.unwrap_or_default(),
				value: self.value.unwrap_or_default(),
				to: self.to,
				gas: self.gas.unwrap_or_default(),
				gas_price: self.gas_price.unwrap_or_default(),
			}
			.into()),
			TYPE_EIP1559 => Ok(Transaction1559Unsigned {
				r#type: TypeEip1559 {},
				chain_id: self.chain_id.unwrap_or_default(),
				input: self.input.to_bytes(),
				nonce: self.nonce.unwrap_or_default(),
				value: self.value.unwrap_or_default(),
				to: self.to,
				gas: self.gas.unwrap_or_default(),
				gas_price: self.max_fee_per_gas.unwrap_or_default(),
				access_list: self.access_list.unwrap_or_default(),
				max_fee_per_gas: self.max_fee_per_gas.unwrap_or_default(),
				max_priority_fee_per_gas: self.max_priority_fee_per_gas.unwrap_or_default(),
			}
			.into()),
			TYPE_EIP2930 => Ok(Transaction2930Unsigned {
				r#type: TypeEip2930 {},
				chain_id: self.chain_id.unwrap_or_default(),
				input: self.input.to_bytes(),
				nonce: self.nonce.unwrap_or_default(),
				value: self.value.unwrap_or_default(),
				to: self.to,
				gas: self.gas.unwrap_or_default(),
				gas_price: self.gas_price.unwrap_or_default(),
				access_list: self.access_list.unwrap_or_default(),
			}
			.into()),
			TYPE_EIP4844 => Ok(Transaction4844Unsigned {
				r#type: TypeEip4844 {},
				chain_id: self.chain_id.unwrap_or_default(),
				input: self.input.to_bytes(),
				nonce: self.nonce.unwrap_or_default(),
				value: self.value.unwrap_or_default(),
				to: self.to.unwrap_or_default(),
				gas: self.gas.unwrap_or_default(),
				max_fee_per_gas: self.max_fee_per_gas.unwrap_or_default(),
				max_fee_per_blob_gas: self.max_fee_per_blob_gas.unwrap_or_default(),
				max_priority_fee_per_gas: self.max_priority_fee_per_gas.unwrap_or_default(),
				access_list: self.access_list.unwrap_or_default(),
				blob_versioned_hashes: self.blob_versioned_hashes,
			}
			.into()),
			TYPE_EIP7702 => Ok(Transaction7702Unsigned {
				r#type: TypeEip7702 {},
				chain_id: self.chain_id.unwrap_or_default(),
				input: self.input.to_bytes(),
				nonce: self.nonce.unwrap_or_default(),
				value: self.value.unwrap_or_default(),
				to: self.to.unwrap_or_default(),
				gas: self.gas.unwrap_or_default(),
				max_fee_per_gas: self.max_fee_per_gas.unwrap_or_default(),
				max_priority_fee_per_gas: self.max_priority_fee_per_gas.unwrap_or_default(),
				access_list: self.access_list.unwrap_or_default(),
				authorization_list: self.authorization_list,
			}
			.into()),
			_ => Err(()),
		}
	}
}

impl From<GenericTransactionV1> for GenericTransaction {
	fn from(value: GenericTransactionV1) -> Self {
		Self {
			access_list: value.access_list.map(|list| list.into_iter().map(Into::into).collect()),
			authorization_list: value.authorization_list.into_iter().map(Into::into).collect(),
			blob_versioned_hashes: value.blob_versioned_hashes,
			blobs: value.blobs,
			chain_id: value.chain_id,
			from: value.from,
			gas: value.gas,
			gas_price: value.gas_price,
			input: value.input.into(),
			max_fee_per_blob_gas: value.max_fee_per_blob_gas,
			max_fee_per_gas: value.max_fee_per_gas,
			max_priority_fee_per_gas: value.max_priority_fee_per_gas,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

impl From<GenericTransaction> for GenericTransactionV1 {
	fn from(value: GenericTransaction) -> Self {
		Self {
			access_list: value.access_list.map(|list| list.into_iter().map(Into::into).collect()),
			authorization_list: value.authorization_list.into_iter().map(Into::into).collect(),
			blob_versioned_hashes: value.blob_versioned_hashes,
			blobs: value.blobs,
			chain_id: value.chain_id,
			from: value.from,
			gas: value.gas,
			gas_price: value.gas_price,
			input: value.input.into(),
			max_fee_per_blob_gas: value.max_fee_per_blob_gas,
			max_fee_per_gas: value.max_fee_per_gas,
			max_priority_fee_per_gas: value.max_priority_fee_per_gas,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

/// Transaction information
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode)]
pub struct TransactionInfo {
	/// block hash
	pub block_hash: H256,
	/// block number
	pub block_number: U256,
	/// from address
	pub from: Address,
	/// transaction hash
	pub hash: H256,
	/// transaction index
	pub transaction_index: U256,
	pub transaction_signed: TransactionSigned,
}

impl From<TransactionInfo> for TransactionInfoV1 {
	fn from(value: TransactionInfo) -> Self {
		Self {
			block_hash: value.block_hash,
			block_number: value.block_number,
			from: value.from,
			hash: value.hash,
			transaction_index: value.transaction_index,
			transaction_signed: value.transaction_signed.into(),
		}
	}
}

#[derive(
	Debug, Clone, From, TryInto, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking,
)]
pub enum TransactionSigned {
	Transaction7702Signed(Transaction7702Signed),
	Transaction4844Signed(Transaction4844Signed),
	Transaction1559Signed(Transaction1559Signed),
	Transaction2930Signed(Transaction2930Signed),
	TransactionLegacySigned(TransactionLegacySigned),
}

impl Default for TransactionSigned {
	fn default() -> Self {
		TransactionSigned::TransactionLegacySigned(Default::default())
	}
}

impl From<TransactionSigned> for TransactionSignedV1 {
	fn from(value: TransactionSigned) -> Self {
		match value {
			TransactionSigned::Transaction7702Signed(tx) => Self::Transaction7702Signed(tx.into()),
			TransactionSigned::Transaction4844Signed(tx) => Self::Transaction4844Signed(tx.into()),
			TransactionSigned::Transaction1559Signed(tx) => Self::Transaction1559Signed(tx.into()),
			TransactionSigned::Transaction2930Signed(tx) => Self::Transaction2930Signed(tx.into()),
			TransactionSigned::TransactionLegacySigned(tx) => {
				Self::TransactionLegacySigned(tx.into())
			},
		}
	}
}

#[derive(Debug, Clone, From, TryInto, Eq, PartialEq)]
pub enum TransactionUnsigned {
	Transaction7702Unsigned(Transaction7702Unsigned),
	Transaction4844Unsigned(Transaction4844Unsigned),
	Transaction1559Unsigned(Transaction1559Unsigned),
	Transaction2930Unsigned(Transaction2930Unsigned),
	TransactionLegacyUnsigned(TransactionLegacyUnsigned),
}

impl Default for TransactionUnsigned {
	fn default() -> Self {
		TransactionUnsigned::TransactionLegacyUnsigned(Default::default())
	}
}

impl From<TransactionSigned> for TransactionUnsigned {
	fn from(tx: TransactionSigned) -> Self {
		use TransactionSigned::*;
		match tx {
			Transaction7702Signed(tx) => tx.transaction_7702_unsigned.into(),
			Transaction4844Signed(tx) => tx.transaction_4844_unsigned.into(),
			Transaction1559Signed(tx) => tx.transaction_1559_unsigned.into(),
			Transaction2930Signed(tx) => tx.transaction_2930_unsigned.into(),
			TransactionLegacySigned(tx) => tx.transaction_legacy_unsigned.into(),
		}
	}
}

/// EIP-1559 transaction.
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction1559Unsigned {
	/// accessList
	/// EIP-2930 access list
	pub access_list: AccessList,
	/// chainId
	/// Chain ID that this transaction is valid on.
	pub chain_id: U256,
	/// gas limit
	pub gas: U256,
	/// gas price
	/// The effective gas price paid by the sender in wei. For transactions not yet included in a
	/// block, this value should be set equal to the max fee per gas. This field is DEPRECATED,
	/// please transition to using effectiveGasPrice in the receipt object going forward.
	pub gas_price: U256,
	/// input data
	pub input: Bytes,
	/// max fee per gas
	/// The maximum total fee per gas the sender is willing to pay (includes the network / base fee
	/// and miner / priority fee) in wei
	pub max_fee_per_gas: U256,
	/// max priority fee per gas
	/// Maximum fee per gas the sender is willing to pay to miners in wei
	pub max_priority_fee_per_gas: U256,
	/// nonce
	pub nonce: U256,
	/// to address
	pub to: Option<Address>,
	/// type
	pub r#type: TypeEip1559,
	/// value
	pub value: U256,
}

impl From<Transaction1559Unsigned> for Transaction1559UnsignedV1 {
	fn from(value: Transaction1559Unsigned) -> Self {
		Self {
			access_list: value.access_list.into_iter().map(Into::into).collect(),
			chain_id: value.chain_id,
			gas: value.gas,
			gas_price: value.gas_price,
			input: value.input,
			max_fee_per_gas: value.max_fee_per_gas,
			max_priority_fee_per_gas: value.max_priority_fee_per_gas,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

/// EIP-2930 transaction.
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction2930Unsigned {
	/// accessList
	/// EIP-2930 access list
	pub access_list: AccessList,
	/// chainId
	/// Chain ID that this transaction is valid on.
	pub chain_id: U256,
	/// gas limit
	pub gas: U256,
	/// gas price
	/// The gas price willing to be paid by the sender in wei
	pub gas_price: U256,
	/// input data
	pub input: Bytes,
	/// nonce
	pub nonce: U256,
	/// to address
	pub to: Option<Address>,
	/// type
	pub r#type: TypeEip2930,
	/// value
	pub value: U256,
}

impl From<Transaction2930Unsigned> for Transaction2930UnsignedV1 {
	fn from(value: Transaction2930Unsigned) -> Self {
		Self {
			access_list: value.access_list.into_iter().map(Into::into).collect(),
			chain_id: value.chain_id,
			gas: value.gas,
			gas_price: value.gas_price,
			input: value.input,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

/// EIP-4844 transaction.
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction4844Unsigned {
	/// accessList
	/// EIP-2930 access list
	pub access_list: AccessList,
	/// blobVersionedHashes
	/// List of versioned blob hashes associated with the transaction's EIP-4844 data blobs.
	pub blob_versioned_hashes: Vec<H256>,
	/// chainId
	/// Chain ID that this transaction is valid on.
	pub chain_id: U256,
	/// gas limit
	pub gas: U256,
	/// input data
	pub input: Bytes,
	/// max fee per blob gas
	/// The maximum total fee per gas the sender is willing to pay for blob gas in wei
	pub max_fee_per_blob_gas: U256,
	/// max fee per gas
	/// The maximum total fee per gas the sender is willing to pay (includes the network / base fee
	/// and miner / priority fee) in wei
	pub max_fee_per_gas: U256,
	/// max priority fee per gas
	/// Maximum fee per gas the sender is willing to pay to miners in wei
	pub max_priority_fee_per_gas: U256,
	/// nonce
	pub nonce: U256,
	/// to address
	pub to: Address,
	/// type
	pub r#type: TypeEip4844,
	/// value
	pub value: U256,
}

impl From<Transaction4844Unsigned> for Transaction4844UnsignedV1 {
	fn from(value: Transaction4844Unsigned) -> Self {
		Self {
			access_list: value.access_list.into_iter().map(Into::into).collect(),
			blob_versioned_hashes: value.blob_versioned_hashes,
			chain_id: value.chain_id,
			gas: value.gas,
			input: value.input,
			max_fee_per_blob_gas: value.max_fee_per_blob_gas,
			max_fee_per_gas: value.max_fee_per_gas,
			max_priority_fee_per_gas: value.max_priority_fee_per_gas,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

/// Legacy transaction.
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct TransactionLegacyUnsigned {
	/// chainId
	/// Chain ID that this transaction is valid on.
	pub chain_id: Option<U256>,
	/// gas limit
	pub gas: U256,
	/// gas price
	/// The gas price willing to be paid by the sender in wei
	pub gas_price: U256,
	/// input data
	pub input: Bytes,
	/// nonce
	pub nonce: U256,
	/// to address
	pub to: Option<Address>,
	/// type
	pub r#type: TypeLegacy,
	/// value
	pub value: U256,
}

impl From<TransactionLegacyUnsigned> for TransactionLegacyUnsignedV1 {
	fn from(value: TransactionLegacyUnsigned) -> Self {
		Self {
			chain_id: value.chain_id,
			gas: value.gas,
			gas_price: value.gas_price,
			input: value.input,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

/// EIP-7702 transaction.
#[derive(
	Debug, Clone, Default, From, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking,
)]
pub struct Transaction7702Unsigned {
	/// accessList
	/// EIP-2930 access list
	pub access_list: AccessList,
	/// authorizationList
	/// List of account code authorizations
	pub authorization_list: Vec<AuthorizationListEntry>,
	/// chainId
	/// Chain ID that this transaction is valid on.
	pub chain_id: U256,
	/// gas limit
	pub gas: U256,
	/// input data
	pub input: Bytes,
	/// max fee per gas
	/// The maximum total fee per gas the sender is willing to pay (includes the network / base fee
	/// and miner / priority fee) in wei
	pub max_fee_per_gas: U256,
	/// max priority fee per gas
	/// Maximum fee per gas the sender is willing to pay to miners in wei
	pub max_priority_fee_per_gas: U256,
	/// nonce
	pub nonce: U256,
	/// to address
	///
	/// # Note
	///
	/// Extracted from eip-7702: `Note, this implies a null destination is not valid.`
	pub to: Address,
	/// type
	pub r#type: TypeEip7702,
	/// value
	pub value: U256,
}

impl From<Transaction7702Unsigned> for Transaction7702UnsignedV1 {
	fn from(value: Transaction7702Unsigned) -> Self {
		Self {
			access_list: value.access_list.into_iter().map(Into::into).collect(),
			authorization_list: value.authorization_list.into_iter().map(Into::into).collect(),
			chain_id: value.chain_id,
			gas: value.gas,
			input: value.input,
			max_fee_per_gas: value.max_fee_per_gas,
			max_priority_fee_per_gas: value.max_priority_fee_per_gas,
			nonce: value.nonce,
			to: value.to,
			r#type: value.r#type,
			value: value.value,
		}
	}
}

/// Signed 7702 Transaction
#[derive(Debug, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction7702Signed {
	pub transaction_7702_unsigned: Transaction7702Unsigned,
	/// r
	pub r: U256,
	/// s
	pub s: U256,
	/// v
	/// For backwards compatibility, `v` is optionally provided as an alternative to `yParity`.
	/// This field is DEPRECATED and all use of it should migrate to `yParity`.
	pub v: Option<U256>,
	/// yParity
	/// The parity (0 for even, 1 for odd) of the y-value of the secp256k1 signature.
	pub y_parity: U256,
}

impl From<Transaction7702Signed> for Transaction7702SignedV1 {
	fn from(value: Transaction7702Signed) -> Self {
		Self {
			transaction_7702_unsigned: value.transaction_7702_unsigned.into(),
			r: value.r,
			s: value.s,
			v: value.v,
			y_parity: value.y_parity,
		}
	}
}

/// Signed 1559 Transaction
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction1559Signed {
	pub transaction_1559_unsigned: Transaction1559Unsigned,
	/// r
	pub r: U256,
	/// s
	pub s: U256,
	/// v
	/// For backwards compatibility, `v` is optionally provided as an alternative to `yParity`.
	/// This field is DEPRECATED and all use of it should migrate to `yParity`.
	pub v: Option<U256>,
	/// yParity
	/// The parity (0 for even, 1 for odd) of the y-value of the secp256k1 signature.
	pub y_parity: U256,
}

impl From<Transaction1559Signed> for Transaction1559SignedV1 {
	fn from(value: Transaction1559Signed) -> Self {
		Self {
			transaction_1559_unsigned: value.transaction_1559_unsigned.into(),
			r: value.r,
			s: value.s,
			v: value.v,
			y_parity: value.y_parity,
		}
	}
}

/// Signed 2930 Transaction
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction2930Signed {
	pub transaction_2930_unsigned: Transaction2930Unsigned,
	/// r
	pub r: U256,
	/// s
	pub s: U256,
	/// v
	/// For backwards compatibility, `v` is optionally provided as an alternative to `yParity`.
	/// This field is DEPRECATED and all use of it should migrate to `yParity`.
	pub v: Option<U256>,
	/// yParity
	/// The parity (0 for even, 1 for odd) of the y-value of the secp256k1 signature.
	pub y_parity: U256,
}

impl From<Transaction2930Signed> for Transaction2930SignedV1 {
	fn from(value: Transaction2930Signed) -> Self {
		Self {
			transaction_2930_unsigned: value.transaction_2930_unsigned.into(),
			r: value.r,
			s: value.s,
			v: value.v,
			y_parity: value.y_parity,
		}
	}
}

/// Signed 4844 Transaction
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct Transaction4844Signed {
	pub transaction_4844_unsigned: Transaction4844Unsigned,
	/// r
	pub r: U256,
	/// s
	pub s: U256,
	/// yParity
	/// The parity (0 for even, 1 for odd) of the y-value of the secp256k1 signature.
	pub y_parity: U256,
}

impl From<Transaction4844Signed> for Transaction4844SignedV1 {
	fn from(value: Transaction4844Signed) -> Self {
		Self {
			transaction_4844_unsigned: value.transaction_4844_unsigned.into(),
			r: value.r,
			s: value.s,
			y_parity: value.y_parity,
		}
	}
}

/// Signed Legacy Transaction
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct TransactionLegacySigned {
	pub transaction_legacy_unsigned: TransactionLegacyUnsigned,
	/// r
	pub r: U256,
	/// s
	pub s: U256,
	/// v
	pub v: U256,
}

impl From<TransactionLegacySigned> for TransactionLegacySignedV1 {
	fn from(value: TransactionLegacySigned) -> Self {
		Self {
			transaction_legacy_unsigned: value.transaction_legacy_unsigned.into(),
			r: value.r,
			s: value.s,
			v: value.v,
		}
	}
}

/// Access list
pub type AccessList = Vec<AccessListEntry>;

/// Access list entry
#[derive(Debug, Default, Clone, Encode, Decode, TypeInfo, Eq, PartialEq, DecodeWithMemTracking)]
pub struct AccessListEntry {
	pub address: Address,
	pub storage_keys: Vec<H256>,
}

impl From<AccessListEntryV1> for AccessListEntry {
	fn from(value: AccessListEntryV1) -> Self {
		Self { address: value.address, storage_keys: value.storage_keys }
	}
}

impl From<AccessListEntry> for AccessListEntryV1 {
	fn from(value: AccessListEntry) -> Self {
		Self { address: value.address, storage_keys: value.storage_keys }
	}
}

/// Authorization list entry for EIP-7702
#[derive(Debug, Default, Clone, Eq, PartialEq, TypeInfo, Encode, Decode, DecodeWithMemTracking)]
pub struct AuthorizationListEntry {
	/// Chain ID that this authorization is valid on
	pub chain_id: U256,
	/// Address to authorize
	pub address: Address,
	/// Nonce of the authorization
	pub nonce: U256,
	/// y-parity of the signature
	pub y_parity: U256,
	/// r component of signature
	pub r: U256,
	/// s component of signature
	pub s: U256,
}

impl From<AuthorizationListEntryV1> for AuthorizationListEntry {
	fn from(value: AuthorizationListEntryV1) -> Self {
		Self {
			chain_id: value.chain_id,
			address: value.address,
			nonce: value.nonce,
			y_parity: value.y_parity,
			r: value.r,
			s: value.s,
		}
	}
}

impl From<AuthorizationListEntry> for AuthorizationListEntryV1 {
	fn from(value: AuthorizationListEntry) -> Self {
		Self {
			chain_id: value.chain_id,
			address: value.address,
			nonce: value.nonce,
			y_parity: value.y_parity,
			r: value.r,
			s: value.s,
		}
	}
}

#[derive(Debug, Clone, From, TryInto, Eq, PartialEq, TypeInfo, Encode, Decode)]
pub enum HashesOrTransactionInfos {
	/// Transaction hashes
	Hashes(Vec<H256>),
	/// Full transactions
	TransactionInfos(Vec<TransactionInfo>),
}

impl Default for HashesOrTransactionInfos {
	fn default() -> Self {
		HashesOrTransactionInfos::Hashes(Default::default())
	}
}

impl From<HashesOrTransactionInfos> for HashesOrTransactionInfosV1 {
	fn from(value: HashesOrTransactionInfos) -> Self {
		match value {
			HashesOrTransactionInfos::Hashes(hashes) => Self::Hashes(hashes),
			HashesOrTransactionInfos::TransactionInfos(infos) => {
				Self::TransactionInfos(infos.into_iter().map(Into::into).collect())
			},
		}
	}
}

impl HashesOrTransactionInfos {
	pub fn push_hash(&mut self, hash: H256) {
		match self {
			HashesOrTransactionInfos::Hashes(hashes) => hashes.push(hash),
			_ => {},
		}
	}

	pub fn len(&self) -> usize {
		match self {
			HashesOrTransactionInfos::Hashes(v) => v.len(),
			HashesOrTransactionInfos::TransactionInfos(v) => v.len(),
		}
	}

	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	pub fn contains_tx(&self, hash: H256) -> bool {
		match self {
			HashesOrTransactionInfos::Hashes(hashes) => hashes.iter().any(|h256| *h256 == hash),
			HashesOrTransactionInfos::TransactionInfos(transaction_infos) => {
				transaction_infos.iter().any(|ti| ti.hash == hash)
			},
		}
	}
}

/// Input of a `GenericTransaction`
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct InputOrData {
	input: Option<Bytes>,
	data: Option<Bytes>,
}

impl From<Bytes> for InputOrData {
	fn from(value: Bytes) -> Self {
		InputOrData { input: Some(value), data: None }
	}
}

impl From<Vec<u8>> for InputOrData {
	fn from(value: Vec<u8>) -> Self {
		InputOrData { input: Some(Bytes(value)), data: None }
	}
}

impl InputOrData {
	/// Get the input as `Bytes`.
	pub fn to_bytes(self) -> Bytes {
		match self {
			InputOrData { input: Some(input), data: _ } => input,
			InputOrData { input: None, data: Some(data) } => data,
			_ => Default::default(),
		}
	}

	/// Get the input as `Vec<u8>`.
	pub fn to_vec(self) -> Vec<u8> {
		self.to_bytes().0
	}

	/// Returns the input as a byte slice, preferring `input` over `data`.
	pub fn as_slice(&self) -> &[u8] {
		self.input
			.as_ref()
			.or(self.data.as_ref())
			.map(|bytes| bytes.0.as_slice())
			.unwrap_or_default()
	}

	/// Returns true if the input carries no bytes.
	pub fn is_empty(&self) -> bool {
		self.as_slice().is_empty()
	}
}

impl From<InputOrDataV1> for InputOrData {
	fn from(value: InputOrDataV1) -> Self {
		Self { input: value.input, data: value.data }
	}
}

impl From<InputOrData> for InputOrDataV1 {
	fn from(value: InputOrData) -> Self {
		Self { input: value.input, data: value.data }
	}
}

#[cfg(test)]
mod tests {
	use crate::evm::*;

	#[test]
	fn from_unsigned_works_for_legacy() {
		let base_gas_price = U256::from(10);
		let tx = TransactionUnsigned::from(TransactionLegacyUnsigned {
			chain_id: Some(U256::from(1)),
			input: Bytes::from(vec![1u8]),
			nonce: U256::from(1),
			value: U256::from(1),
			to: Some(H160::zero()),
			gas: U256::from(1),
			gas_price: U256::from(10),
			..Default::default()
		});

		let generic = GenericTransaction::from_unsigned(tx.clone(), base_gas_price, None);
		assert_eq!(generic.gas_price, Some(U256::from(10)));

		let tx2 = generic.try_into_unsigned().unwrap();
		assert_eq!(tx, tx2);
	}

	#[test]
	fn from_unsigned_works_for_1559() {
		let base_gas_price = U256::from(10);
		let tx = TransactionUnsigned::from(Transaction1559Unsigned {
			chain_id: U256::from(1),
			input: Bytes::from(vec![1u8]),
			nonce: U256::from(1),
			value: U256::from(1),
			to: Some(H160::zero()),
			gas: U256::from(1),
			gas_price: U256::from(20),
			max_fee_per_gas: U256::from(20),
			max_priority_fee_per_gas: U256::from(1),
			..Default::default()
		});

		let generic = GenericTransaction::from_unsigned(tx.clone(), base_gas_price, None);
		assert_eq!(generic.gas_price, Some(U256::from(10)));

		let tx2 = generic.try_into_unsigned().unwrap();
		assert_eq!(tx, tx2);
	}

	#[test]
	fn from_unsigned_works_for_7702() {
		let base_gas_price = U256::from(10);
		let tx = TransactionUnsigned::from(Transaction7702Unsigned {
			chain_id: U256::from(1),
			input: Bytes::from(vec![1u8]),
			nonce: U256::from(1),
			value: U256::from(1),
			to: H160::zero(),
			gas: U256::from(1),
			max_fee_per_gas: U256::from(20),
			max_priority_fee_per_gas: U256::from(1),
			authorization_list: vec![AuthorizationListEntry {
				chain_id: U256::from(1),
				address: H160::from_low_u64_be(42),
				nonce: U256::from(0),
				y_parity: U256::from(1),
				r: U256::from(1),
				s: U256::from(2),
			}],
			..Default::default()
		});

		let generic = GenericTransaction::from_unsigned(tx.clone(), base_gas_price, None);
		assert_eq!(generic.gas_price, Some(U256::from(10)));

		let tx2 = generic.try_into_unsigned().unwrap();
		assert_eq!(tx, tx2);
	}
}
