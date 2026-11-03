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
use serde::{Deserialize, Serialize};
use sp_core::{H160, H256, U256};

use crate::{common::*, runtime_api::*};

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct BlockV1 {
	pub base_fee_per_gas: U256,
	pub blob_gas_used: U256,
	pub difficulty: U256,
	pub excess_blob_gas: U256,
	pub extra_data: Bytes,
	pub gas_limit: U256,
	pub gas_used: U256,
	pub hash: H256,
	pub logs_bloom: Bytes256,
	pub miner: H160,
	pub mix_hash: H256,
	pub nonce: Bytes8,
	pub number: U256,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub parent_beacon_block_root: Option<H256>,
	pub parent_hash: H256,
	pub receipts_root: H256,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub requests_hash: Option<H256>,
	pub sha_3_uncles: H256,
	pub size: U256,
	pub state_root: H256,
	pub timestamp: U256,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub total_difficulty: Option<U256>,
	pub transactions: HashesOrTransactionInfosV1,
	pub transactions_root: H256,
	pub uncles: Vec<H256>,
	pub withdrawals: Vec<WithdrawalV1>,
	pub withdrawals_root: H256,
}

#[derive(
	Debug, Default, Clone, Serialize, Deserialize, Eq, PartialEq, TypeInfo, Encode, Decode,
)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawalV1 {
	pub address: H160,
	pub amount: U256,
	pub index: U256,
	pub validator_index: U256,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_block_serialization_roundtrip() {
		let json_input = r#"{
			"baseFeePerGas": "0x126f2347",
			"blobGasUsed": "0x100000",
			"difficulty": "0x0",
			"excessBlobGas": "0x0",
			"extraData": "0x546974616e2028746974616e6275696c6465722e78797a29",
			"gasLimit": "0x2aca2c9",
			"gasUsed": "0x1c06043",
			"hash": "0xe6064637def8a5a9a90c8a666005975e4a6c46acf8af57e1f2adb20dfced133a",
			"logsBloom": "0xbf7bf1afcf57ea95fbb5c6fd8db37db9dbffec27cfc6a39b3417e7786defd7e3d6fd577ecddd5676eee8bf79df8faddcefa7e169def77f7e7d6dbbfd1dfef9aebd9e707b4c4ed979fda2cdeeb96b3bfed5d5fabb68ff9e7f2dfb075eff643a93feebbc07877f0dff66fedf4ede0fbcfbf56f98a1626eaed77ed4e6be388f162f9b2deeff1eefa93bdacbf3fbbd7b6757cddb7ae5b3f9b7af9c3bbff7e7f6ddef9f2dff7f17997ea6867675c29fcbe6bf725efbffe1507589bfd47a3bf7b6f5dfde50776fd94fe772d2c7b6b58baf554de55c176f27efa6fdcff7f17689bafa7f7c7bf4fd5fb9b05c2f4ed785f17ac9779feeaf1f5bbdadfc42ebad367fdcf7ad",
			"miner": "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97",
			"mixHash": "0x7e53d2d6772895d024eb00da80213aec81fb4a15bec34a5a39403ad6162274af",
			"nonce": "0x0000000000000000",
			"number": "0x1606672",
			"parentBeaconBlockRoot": "0xd9ef51c8f4155f238ba66df0d35a4d0a6bb043c0dacb5c5dbd5a231bbd4c8a01",
			"parentHash": "0x37b527c98c86436f292d4e19fac3aba6d8c7768684ea972f50adc305fd9a1475",
			"receiptsRoot": "0x2abab67c41b350435eb34f9dc0478dd7d262f35544cecf62a85af2da075bd38d",
			"requestsHash": "0xe3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
			"sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
			"size": "0x29e6c",
			"stateRoot": "0x5159c56472adff9a760275ac63524a71f5645ede822a5547dd9ad333586d5157",
			"timestamp": "0x6895a93f",
			"transactions": [],
			"transactionsRoot": "0xfb0b9f5b28bc927db98d82e18070d2b17434c31bd2773c5dd699e96fa76a34cd",
			"uncles": [],
			"withdrawals": [],
			"withdrawalsRoot": "0x531480435633d56a52433b33f41ac9322f51a2df3364c4c112236fc6ac583118"
		}"#;

		let block: BlockV1 = serde_json::from_str(json_input).expect("Failed to deserialize block");

		let serialized = serde_json::to_string(&block).expect("Failed to serialize block");

		let block_roundtrip: BlockV1 =
			serde_json::from_str(&serialized).expect("Failed to deserialize roundtrip block");

		assert_eq!(block, block_roundtrip);
	}

	#[test]
	fn test_block_decode() {
		let json = r#"{
			"baseFeePerGas": "0x23cf3fd4",
			"blobGasUsed": "0x0",
			"difficulty": "0x0",
			"excessBlobGas": "0x80000",
			"extraData": "0x546974616e2028746974616e6275696c6465722e78797a29",
			"gasLimit": "0x2aea4ea",
			"gasUsed": "0xe36e2f",
			"hash": "0xfb8c980d1da1a75e68c2ea4d55cb88d62dedbbb5eaf69df8fe337e9f6922b73a",
			"logsBloom": "0xb56c514421c05ba024436428e2487b83134983e9c650686421bd10588512e0a9a55d51e8e84c868446517ed5e90609dd43aad1edcc1462b8e8f15763b3ff6e62a506d3d910d0aae829786fac994a6de34860263be47eb8300e91dd2cc3110a22ba0d60008e6a0362c5a3ffd5aa18acc8c22b6fe02c54273b12a841bc958c9ae12378bc0e5881c2d840ff677f8038243216e5c105e58819bc0cbb8c56abb7e490cf919ceb85702e5d54dece9332a00c9e6ade9cb47d42440201ecd7704088236b39037c9ff189286e3e5d6657aa389c2d482e337af5cfc45b0d25ad0e300c2b6bf599bc2007008830226612a4e7e7cae4e57c740205a809dc280825165b98559c",
			"miner": "0x4838b106fce9647bdf1e7877bf73ce8b0bad5f97",
			"mixHash": "0x11b02e97eaa48bc83cbb6f9478f32eaf7e8b67fead4edeef945822612f1854f6",
			"nonce": "0x0000000000000000",
			"number": "0x161bd0f",
			"parentBeaconBlockRoot": "0xd8266eb7bb40e4e5e3beb9caed7ccaa448ce55203a03705c87860deedcf7236d",
			"parentHash": "0x7c9625cc198af5cf677a15cdc38da3cf64d57b9729de5bd1c96b3c556a84aa7d",
			"receiptsRoot": "0x758614638725ede86a2f4c8339eb79b84ae346915319dc286643c9324e34f28a",
			"requestsHash": "0xd9267a5ab4782c4e0bdc5fcd2fefb53c91f92f91b6059c8f13343a0691ba77d1",
			"sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
			"size": "0x14068",
			"stateRoot": "0x7ed9726e3172886af5301968c2ddb7c38f8adf99c99ec10fdfaab66c610854bb",
			"timestamp": "0x68a5ce5b",
			"transactions": [
				{
				"blockHash": "0xfb8c980d1da1a75e68c2ea4d55cb88d62dedbbb5eaf69df8fe337e9f6922b73a",
				"blockNumber": "0x161bd0f",
				"from": "0x693ca5c6852a7d212dabc98b28e15257465c11f3",
				"gas": "0x70bdb",
				"gasPrice": "0x23cf3fd4",
				"maxPriorityFeePerGas": "0x0",
				"maxFeePerGas": "0x47ca802f",
				"hash": "0xf6d8b07ddcf9a9d44c99c3665fd8c78f0ccd32506350ea5a9be1a68ba08bfd1f",
				"input": "0x09c5eabe000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000002a90000cca0b86991c6218b36c1d19d4a2e9eb0ce3606eb48000000000000000000000000000000020000000000000000000000035c9618f600000000000000000000000000000000c02aaa39b223fe8d0a0e5c4f27ead9083c756cc20000000000000000000000002374fed200000000000000000001528fd550bc9a0000000000000000351e55bea6d51900dac17f958d2ee523a2206206994597c13d831ec7000000000000000000000000000000000000000000000000000000005c0c965e0000000000000000000000000000000000004c00000001000000000000000000000000000000000000002e24cd1d61a63f43658ed73b6ddeba00010002000100000000000000000000000000000000000000000000000039d622818daae62900006602000000000000000000002ff9e9686fa6ac00000000000000000000000000007f88ca000000000000000004caaa5ba8029c920300010000000000000000052319c661ddb06600000000000000000001528fd550bc9a0000000000000000005049606b67676100011c0c00000000000000002ff9e9686fa6ac000000000000000000000000035c16902c0000000000000000000000000000000200000000000000000000000000000002000073d53553ee552c1f2a9722e6407d43e41e19593f1cbc3d63300bfc6e48709f5b5ed98f228c70104e8c5d570b5608b47dca95ce6e371636965b6fdcab3613b6b65f061a44b7132011bb97a768bd238eacb62d7109920b000000000000000005246c56372e6d000000000000000000000000005c0c965e0000000000000000000000002374fed20000000000000000000000002374fed200011cc19621f6edbb9c02b95055b9f52eba0e2cb954c259f42aeca488551ea82b72f2504bbd310eb7145435e258751ab6854ab08b1630b89d6621dc1398c5d0c43b480000000000000000000000000000000000000000000000000000",
				"nonce": "0x40c6",
				"to": "0x0000000aa232009084bd71a5797d089aa4edfad4",
				"transactionIndex": "0x0",
				"value": "0x0",
				"type": "0x2",
				"accessList": [],
				"chainId": "0x1",
				"v": "0x1",
				"yParity": "0x1",
				"r": "0xb3e71bd95d73e965495b17647f5faaf058e13af7dd21f2af24eac16f7e9d06a1",
				"s": "0x58775b0c15075fb7f007b88e88605ae5daec1ffbac2771076e081c8c2b005c20"
				},
				{
				"blockHash": "0xfb8c980d1da1a75e68c2ea4d55cb88d62dedbbb5eaf69df8fe337e9f6922b73a",
				"blockNumber": "0x161bd0f",
				"from": "0x4791eb2224d272655e8d5da171bb07dd5a805ff6",
				"hash": "0xda8bc5dc5617758c6af0681d71642f68ce679bb92df4d8cf48493f0cfad14e20",
				"transactionIndex": "0x19",
				"gas": "0x186a0",
				"gasPrice": "0x6a5efc76",
				"maxPriorityFeePerGas": "0x6a5efc76",
				"maxFeePerGas": "0x6a5efc76",
				"input": "0x2c7bddf4",
				"nonce": "0x6233",
				"to": "0x62b53c45305d29bbe4b1bfa49dd78766b2f1e624",
				"value": "0x0",
				"type": "0x4",
				"accessList": [],
				"chainId": "0x1",
				"authorizationList": [
				],
				"v": "0x1",
				"yParity": "0x1",
				"r": "0x3b863c04d39f70e499ffb176376128a57481727116027a92a364b6e1668d13a7",
				"s": "0x39b13f0597c509de8260c7808057e64126e7d0715044dda908d1f513e1ed79ad"
				}
			],
			"transactionsRoot": "0xca2e7e6ebe1b08030fe5b9efabee82b95e62f07cff5a4298354002c46b41a216",
			"uncles": [],
			"withdrawals": [
			],
			"withdrawalsRoot": "0x7a3ad42fdb774c0e662597141f52a81210ffec9ce0db9dfcd841f747b0909010"
		}"#;

		let _result: BlockV1 = serde_json::from_str(json).unwrap();
	}
}
