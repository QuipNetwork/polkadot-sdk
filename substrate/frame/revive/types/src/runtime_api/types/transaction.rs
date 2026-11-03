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

use alloc::vec::Vec;
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use serde::{Deserialize, Deserializer, Serialize};
use sp_core::{H160, H256, U256};

use crate::common::*;

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
#[serde(rename_all = "camelCase")]
pub struct GenericTransactionV1 {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub access_list: Option<AccessListV1>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub authorization_list: Vec<AuthorizationListEntryV1>,
	#[serde(default)]
	pub blob_versioned_hashes: Vec<H256>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub blobs: Vec<Bytes>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub chain_id: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub from: Option<H160>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub gas: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub gas_price: Option<U256>,
	#[serde(flatten, deserialize_with = "deserialize_input_or_data")]
	pub input: InputOrDataV1,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_fee_per_blob_gas: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_fee_per_gas: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub max_priority_fee_per_gas: Option<U256>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub nonce: Option<U256>,
	pub to: Option<H160>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub r#type: Option<Byte>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub value: Option<U256>,
}

#[derive(Debug, Default, Clone, Serialize, Eq, PartialEq, TypeInfo, Encode, Decode)]
#[serde(rename_all = "camelCase")]
pub struct TransactionInfoV1 {
	pub block_hash: H256,
	pub block_number: U256,
	pub from: H160,
	pub hash: H256,
	pub transaction_index: U256,
	#[serde(flatten)]
	pub transaction_signed: TransactionSignedV1,
}

impl<'de> Deserialize<'de> for TransactionInfoV1 {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		use alloc::{collections::BTreeMap, string::String};
		use serde::de::Error;

		let mut map = <BTreeMap<String, serde_json::Value>>::deserialize(deserializer)?;

		let block_hash =
			map.remove("blockHash").ok_or_else(|| D::Error::missing_field("blockHash"))?;
		let block_number = map
			.remove("blockNumber")
			.ok_or_else(|| D::Error::missing_field("blockNumber"))?;
		let from = map.remove("from").ok_or_else(|| D::Error::missing_field("from"))?;
		let hash = map.remove("hash").ok_or_else(|| D::Error::missing_field("hash"))?;
		let transaction_index = map
			.remove("transactionIndex")
			.ok_or_else(|| D::Error::missing_field("transactionIndex"))?;

		let remaining = serde_json::Value::Object(map.into_iter().collect());
		let json_str = serde_json::to_string(&remaining).map_err(D::Error::custom)?;
		let transaction_signed: TransactionSignedV1 =
			serde_json::from_str(&json_str).map_err(D::Error::custom)?;

		Ok(Self {
			block_hash: serde_json::from_value(block_hash).map_err(D::Error::custom)?,
			block_number: serde_json::from_value(block_number).map_err(D::Error::custom)?,
			from: serde_json::from_value(from).map_err(D::Error::custom)?,
			hash: serde_json::from_value(hash).map_err(D::Error::custom)?,
			transaction_index: serde_json::from_value(transaction_index)
				.map_err(D::Error::custom)?,
			transaction_signed,
		})
	}
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode)]
#[serde(untagged)]
pub enum TransactionSignedV1 {
	Transaction7702Signed(Transaction7702SignedV1),
	Transaction4844Signed(Transaction4844SignedV1),
	Transaction1559Signed(Transaction1559SignedV1),
	Transaction2930Signed(Transaction2930SignedV1),
	TransactionLegacySigned(TransactionLegacySignedV1),
}

impl Default for TransactionSignedV1 {
	fn default() -> Self {
		TransactionSignedV1::TransactionLegacySigned(Default::default())
	}
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction1559UnsignedV1 {
	pub access_list: AccessListV1,
	pub chain_id: U256,
	pub gas: U256,
	pub gas_price: U256,
	pub input: Bytes,
	pub max_fee_per_gas: U256,
	pub max_priority_fee_per_gas: U256,
	pub nonce: U256,
	pub to: Option<H160>,
	pub r#type: TypeEip1559,
	pub value: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction2930UnsignedV1 {
	pub access_list: AccessListV1,
	pub chain_id: U256,
	pub gas: U256,
	pub gas_price: U256,
	pub input: Bytes,
	pub nonce: U256,
	pub to: Option<H160>,
	pub r#type: TypeEip2930,
	pub value: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction4844UnsignedV1 {
	pub access_list: AccessListV1,
	pub blob_versioned_hashes: Vec<H256>,
	pub chain_id: U256,
	pub gas: U256,
	pub input: Bytes,
	pub max_fee_per_blob_gas: U256,
	pub max_fee_per_gas: U256,
	pub max_priority_fee_per_gas: U256,
	pub nonce: U256,
	pub to: H160,
	pub r#type: TypeEip4844,
	pub value: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct TransactionLegacyUnsignedV1 {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub chain_id: Option<U256>,
	pub gas: U256,
	pub gas_price: U256,
	pub input: Bytes,
	pub nonce: U256,
	pub to: Option<H160>,
	pub r#type: TypeLegacy,
	pub value: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction7702UnsignedV1 {
	pub access_list: AccessListV1,
	pub authorization_list: Vec<AuthorizationListEntryV1>,
	pub chain_id: U256,
	pub gas: U256,
	pub input: Bytes,
	pub max_fee_per_gas: U256,
	pub max_priority_fee_per_gas: U256,
	pub nonce: U256,
	pub to: H160,
	pub r#type: TypeEip7702,
	pub value: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode)]
#[serde(rename_all = "camelCase")]
pub struct Transaction7702SignedV1 {
	#[serde(flatten)]
	pub transaction_7702_unsigned: Transaction7702UnsignedV1,
	pub r: U256,
	pub s: U256,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub v: Option<U256>,
	pub y_parity: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction1559SignedV1 {
	#[serde(flatten)]
	pub transaction_1559_unsigned: Transaction1559UnsignedV1,
	pub r: U256,
	pub s: U256,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub v: Option<U256>,
	pub y_parity: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction2930SignedV1 {
	#[serde(flatten)]
	pub transaction_2930_unsigned: Transaction2930UnsignedV1,
	pub r: U256,
	pub s: U256,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub v: Option<U256>,
	pub y_parity: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct Transaction4844SignedV1 {
	#[serde(flatten)]
	pub transaction_4844_unsigned: Transaction4844UnsignedV1,
	pub r: U256,
	pub s: U256,
	pub y_parity: U256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct TransactionLegacySignedV1 {
	#[serde(flatten)]
	pub transaction_legacy_unsigned: TransactionLegacyUnsignedV1,
	pub r: U256,
	pub s: U256,
	pub v: U256,
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode)]
#[serde(untagged)]
pub enum HashesOrTransactionInfosV1 {
	Hashes(Vec<H256>),
	TransactionInfos(Vec<TransactionInfoV1>),
}

impl Default for HashesOrTransactionInfosV1 {
	fn default() -> Self {
		HashesOrTransactionInfosV1::Hashes(Default::default())
	}
}

impl HashesOrTransactionInfosV1 {
	pub fn push_hash(&mut self, hash: H256) {
		if let HashesOrTransactionInfosV1::Hashes(hashes) = self {
			hashes.push(hash)
		}
	}

	pub fn len(&self) -> usize {
		match self {
			HashesOrTransactionInfosV1::Hashes(hashes) => hashes.len(),
			HashesOrTransactionInfosV1::TransactionInfos(infos) => infos.len(),
		}
	}

	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	pub fn contains_tx(&self, hash: H256) -> bool {
		match self {
			HashesOrTransactionInfosV1::Hashes(hashes) => hashes.iter().any(|h256| *h256 == hash),
			HashesOrTransactionInfosV1::TransactionInfos(transaction_infos) => {
				transaction_infos.iter().any(|info| info.hash == hash)
			},
		}
	}
}

pub type AccessListV1 = Vec<AccessListEntryV1>;

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
#[serde(rename_all = "camelCase")]
pub struct AccessListEntryV1 {
	pub address: H160,
	pub storage_keys: Vec<H256>,
}

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationListEntryV1 {
	pub chain_id: U256,
	pub address: H160,
	pub nonce: U256,
	pub y_parity: U256,
	pub r: U256,
	pub s: U256,
}

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
pub struct InputOrDataV1 {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub input: Option<Bytes>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub data: Option<Bytes>,
}

impl From<Bytes> for InputOrDataV1 {
	fn from(value: Bytes) -> Self {
		InputOrDataV1 { input: Some(value), data: None }
	}
}

impl From<Vec<u8>> for InputOrDataV1 {
	fn from(value: Vec<u8>) -> Self {
		InputOrDataV1 { input: Some(Bytes(value)), data: None }
	}
}

impl InputOrDataV1 {
	pub fn to_bytes(self) -> Bytes {
		match self {
			InputOrDataV1 { input: Some(input), data: _ } => input,
			InputOrDataV1 { input: None, data: Some(data) } => data,
			_ => Default::default(),
		}
	}

	pub fn to_vec(self) -> Vec<u8> {
		self.to_bytes().0
	}

	pub fn as_slice(&self) -> &[u8] {
		self.input
			.as_ref()
			.or(self.data.as_ref())
			.map(|bytes| bytes.0.as_slice())
			.unwrap_or_default()
	}

	pub fn is_empty(&self) -> bool {
		self.as_slice().is_empty()
	}
}

fn deserialize_input_or_data<'d, D: Deserializer<'d>>(d: D) -> Result<InputOrDataV1, D::Error> {
	let value = InputOrDataV1::deserialize(d)?;
	match &value {
		InputOrDataV1 { input: Some(input), data: Some(data) } if input != data => {
			Err(serde::de::Error::custom(
				"Both \"data\" and \"input\" are set and not equal. Please use \"input\" to pass transaction call data",
			))
		},
		_ => Ok(value),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use alloc::{string::ToString, vec};

	#[test]
	fn can_deserialize_input_or_data_field_from_generic_transaction() {
		let cases = [
			("with input", r#"{"input": "0x01"}"#),
			("with data", r#"{"data": "0x01"}"#),
			("with both", r#"{"data": "0x01", "input": "0x01"}"#),
		];

		for (name, json) in cases {
			let tx = serde_json::from_str::<GenericTransactionV1>(json).unwrap();
			assert_eq!(tx.input.to_vec(), vec![1u8], "{}", name);
		}

		let err =
			serde_json::from_str::<GenericTransactionV1>(r#"{"data": "0x02", "input": "0x01"}"#)
				.unwrap_err();
		assert!(
			err.to_string().starts_with(
			"Both \"data\" and \"input\" are set and not equal. Please use \"input\" to pass transaction call data"
			)
		);
	}
	#[test]
	fn test_transaction_info_deserialize_from_value() {
		// This tests the custom deserializer for TransactionInfoV1
		// which works around serde's limitation with flatten + untagged enums from Value
		let tx_info_expected = serde_json::json!({
			"blockHash": "0xfb8c980d1da1a75e68c2ea4d55cb88d62dedbbb5eaf69df8fe337e9f6922b73a",
			"blockNumber": "0x161bd0f",
			"from": "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97",
			"hash": "0x2c522d01183e9ed70caaf75c940ba9908d573cfc9996b3e7adc90313798279c8",
			"transactionIndex": "0x7a",
			"chainId": "0x1",
			"gas": "0x565f",
			"gasPrice": "0x23cf3fd4",
			"input": "0x",
			"nonce": "0x2c5ce1",
			"r": "0x4a5703e4d8daf045f021cb32897a25b17d61b9ab629a59f0731ef4cce63f93d6",
			"s": "0x711812237c1fed6aaf08e9f47fc47e547fdaceba9ab7507e62af29a945354fb6",
			"to": "0x388c818ca8b9251b393131c08a736a67ccb19297",
			"type": "0x0",
			"v": "0x1",
			"value": "0x12bf92aae0c2e70"
		});

		// Test deserializing from Value (this was failing before the custom deserializer) with
		// below error:
		// ```
		// Failed to deserialize from Value: Some(Error("data did not match any variant of untagged enum TransactionSignedV1", line: 0, column: 0))
		// ```
		let tx_info_from_value: Result<TransactionInfoV1, serde_json::Error> =
			serde_json::from_value(tx_info_expected.clone());
		assert!(
			tx_info_from_value.is_ok(),
			"Failed to deserialize from Value: {:?}",
			tx_info_from_value.err()
		);

		// Test deserializing from string (this was always working)
		let json_str = serde_json::to_string(&tx_info_expected).unwrap();
		let tx_info_from_str: Result<TransactionInfoV1, serde_json::Error> =
			serde_json::from_str(&json_str);
		assert!(
			tx_info_from_str.is_ok(),
			"Failed to deserialize from string: {:?}",
			tx_info_from_str.err()
		);

		// Verify both methods produce the same result
		let tx_info_from_value = tx_info_from_value.unwrap();
		let tx_info_from_str = tx_info_from_str.unwrap();
		assert_eq!(
			tx_info_from_value, tx_info_from_str,
			"Value and string deserialization should match"
		);

		// Serialize it back to JSON
		let tx_info_serialized = serde_json::to_value(&tx_info_from_value);
		assert!(
			tx_info_serialized.is_ok(),
			"Failed to serialize to value: {:?}",
			tx_info_serialized.err()
		);
		let tx_info_serialized = tx_info_serialized.unwrap();

		// Verify that deserializing and serializing leads to the same result
		assert_eq!(tx_info_serialized, tx_info_expected);
	}

	#[test]
	fn test_transaction_hashes_deserialization() {
		let json = r#"["0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef"]"#;
		let result: HashesOrTransactionInfosV1 = serde_json::from_str(json).unwrap();
		assert!(matches!(result, HashesOrTransactionInfosV1::Hashes(_)));

		let json = r#"[]"#;
		let result: HashesOrTransactionInfosV1 = serde_json::from_str(json).unwrap();
		assert!(matches!(result, HashesOrTransactionInfosV1::Hashes(_)));

		let json = r#"[{"invalid": "data"}]"#;
		let result: Result<HashesOrTransactionInfosV1, _> = serde_json::from_str(json);
		assert!(result.is_err());
	}

	#[test]
	fn test_transaction_infos_deserialization() {
		let json = r#"[{
			"accessList": [{
				"address": "0x9008d19f58aabd9ed0d60971565aa8510560ab41",
				"storageKeys": [
					"0x0000000000000000000000000000000000000000000000000000000000000001"
				]
			}],
			"blockHash": "0xfb8c980d1da1a75e68c2ea4d55cb88d62dedbbb5eaf69df8fe337e9f6922b73a",
			"blockNumber": "0x161bd0f",
			"chainId": "0x1",
			"from": "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97",
			"gas": "0x565f",
			"gasPrice": "0x23cf3fd4",
			"hash": "0x2c522d01183e9ed70caaf75c940ba9908d573cfc9996b3e7adc90313798279c8",
			"input": "0x",
			"maxFeePerGas": "0x23cf3fd4",
			"maxPriorityFeePerGas": "0x0",
			"nonce": "0x2c5ce1",
			"r": "0x4a5703e4d8daf045f021cb32897a25b17d61b9ab629a59f0731ef4cce63f93d6",
			"s": "0x711812237c1fed6aaf08e9f47fc47e547fdaceba9ab7507e62af29a945354fb6",
			"to": "0x388c818ca8b9251b393131c08a736a67ccb19297",
			"transactionIndex": "0x7a",
			"type": "0x2",
			"v": "0x0",
			"value": "0x12bf92aae0c2e70",
			"yParity": "0x0"
			}]
		"#;
		let result: HashesOrTransactionInfosV1 = serde_json::from_str(json).unwrap();
		assert!(matches!(result, HashesOrTransactionInfosV1::TransactionInfos(_)));
	}
}
