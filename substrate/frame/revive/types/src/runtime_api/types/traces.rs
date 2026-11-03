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

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use codec::{Decode, Encode};
use derive_more::From;
use revm_bytecode::opcode::OpCode;
use scale_info::TypeInfo;
use serde::{Deserialize, Serialize};
use sp_core::{H160, H256, U256};
use sp_weights::Weight;

use crate::common::*;

#[derive(TypeInfo, Deserialize, Serialize, From, Encode, Decode, Clone, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum TraceV1 {
	Call(CallTraceV1),
	Prestate(PrestateTraceV1),
	Execution(ExecutionTraceV1),
}

#[derive(TypeInfo, Deserialize, Serialize, From, Encode, Decode, Clone, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum TraceV2 {
	Call(CallTraceV2),
	Prestate(PrestateTraceV1),
	Execution(ExecutionTraceV1),
}

/// A single extrinsic's trace, or a signal that the runtime could not produce one. An absent entry
/// means there was nothing to trace.
#[derive(TypeInfo, Deserialize, Serialize, From, Encode, Decode, Clone, Debug, Eq, PartialEq)]
pub enum TraceEntryV1 {
	Traced(TraceV2),
	NotTraced,
}

#[derive(
	TypeInfo, Default, Encode, Decode, Serialize, Deserialize, Clone, Debug, Eq, PartialEq,
)]
#[serde(rename_all = "camelCase")]
pub struct CallTraceV1 {
	pub from: H160,
	#[serde(with = "hex_serde")]
	pub gas: u64,
	#[serde(with = "hex_serde")]
	pub gas_used: u64,
	pub to: H160,
	pub input: Bytes,
	#[serde(default, skip_serializing_if = "Bytes::is_empty")]
	pub output: Bytes,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub revert_reason: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub calls: Vec<Self>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub logs: Vec<CallLogV1>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub value: Option<U256>,
	#[serde(rename = "type")]
	pub call_type: CallTypeV1,
	#[serde(skip)]
	pub child_call_count: u32,
}

#[derive(
	TypeInfo, Default, Encode, Decode, Serialize, Deserialize, Clone, Debug, Eq, PartialEq,
)]
#[serde(rename_all = "camelCase")]
pub struct CallTraceV2 {
	pub from: H160,
	#[serde(with = "hex_serde")]
	pub gas: u64,
	#[serde(with = "hex_serde")]
	pub gas_used: u64,
	pub to: H160,
	pub input: Bytes,
	#[serde(default, skip_serializing_if = "Bytes::is_empty")]
	pub output: Bytes,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub revert_reason: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub calls: Vec<Self>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub logs: Vec<CallLogV2>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub value: Option<U256>,
	#[serde(rename = "type")]
	pub call_type: CallTypeV1,
}

#[derive(TypeInfo, Encode, Serialize, Deserialize, Decode, Clone, Debug, Eq, PartialEq)]
#[serde(untagged, deny_unknown_fields)]
pub enum PrestateTraceV1 {
	Prestate(BTreeMap<H160, PrestateTraceInfoV1>),
	DiffMode { pre: BTreeMap<H160, PrestateTraceInfoV1>, post: BTreeMap<H160, PrestateTraceInfoV1> },
}

#[derive(
	Default, TypeInfo, Encode, Decode, Serialize, Deserialize, Clone, Debug, Eq, PartialEq,
)]
#[serde(default, rename_all = "camelCase")]
pub struct ExecutionTraceV1 {
	pub gas: u64,
	pub weight_consumed: Weight,
	pub base_call_weight: Weight,
	pub failed: bool,
	pub return_value: Bytes,
	pub struct_logs: Vec<ExecutionStepV1>,
}

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
pub struct CallLogV1 {
	pub address: H160,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub topics: Vec<H256>,
	pub data: Bytes,
	#[serde(with = "hex_serde")]
	pub position: u32,
}

#[derive(
	Debug, Default, Clone, Encode, Decode, TypeInfo, Serialize, Deserialize, Eq, PartialEq,
)]
pub struct CallLogV2 {
	pub address: H160,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub topics: Vec<H256>,
	pub data: Bytes,
	#[serde(with = "hex_serde")]
	pub position: u32,
	#[serde(with = "hex_serde")]
	pub index: u32,
}

#[derive(
	Default, TypeInfo, Encode, Decode, Serialize, Deserialize, Eq, PartialEq, Clone, Debug,
)]
#[serde(rename_all = "UPPERCASE")]
pub enum CallTypeV1 {
	#[default]
	Call,
	StaticCall,
	DelegateCall,
	Create,
	Create2,
	Selfdestruct,
}

#[derive(
	TypeInfo, Default, Encode, Decode, Serialize, Deserialize, Clone, Debug, Eq, PartialEq,
)]
pub struct PrestateTraceInfoV1 {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub balance: Option<U256>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub nonce: Option<u32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub code: Option<Bytes>,
	#[serde(
		default,
		skip_serializing_if = "option_value_map::is_empty",
		serialize_with = "option_value_map::serialize_skip_none"
	)]
	pub storage: BTreeMap<Bytes, Option<Bytes>>,
}

#[derive(
	TypeInfo, Encode, Decode, Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Default,
)]
#[serde(default, rename_all = "camelCase")]
pub struct ExecutionStepV1 {
	#[codec(compact)]
	pub gas: u64,
	#[codec(compact)]
	pub gas_cost: u64,
	pub weight_cost: Weight,
	pub depth: u16,
	#[serde(default, skip_serializing_if = "Bytes::is_empty")]
	pub return_data: Bytes,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<String>,
	#[serde(flatten)]
	pub kind: ExecutionStepKindV1,
}

#[derive(TypeInfo, Encode, Decode, Serialize, Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(untagged)]
pub enum ExecutionStepKindV1 {
	EVMOpcode {
		#[codec(compact)]
		pc: u32,
		op: EvmOpcodeV1,
		#[serde(
			default,
			serialize_with = "serialize_stack_minimal",
			deserialize_with = "deserialize_stack_minimal"
		)]
		stack: Vec<Bytes>,
		#[serde(
			default,
			skip_serializing_if = "Vec::is_empty",
			serialize_with = "serialize_memory_no_prefix"
		)]
		memory: Vec<Bytes>,
		#[serde(
			default,
			skip_serializing_if = "Option::is_none",
			serialize_with = "serialize_storage_no_prefix"
		)]
		storage: Option<alloc::collections::BTreeMap<Bytes, Bytes>>,
	},
	PVMSyscall {
		op: PolkavmSyscallV1,
		#[serde(default, skip_serializing_if = "Vec::is_empty", with = "hex_serde::vec")]
		args: Vec<u64>,
		#[serde(default, skip_serializing_if = "Option::is_none", with = "hex_serde::option")]
		returned: Option<u64>,
	},
}

impl Default for ExecutionStepKindV1 {
	fn default() -> Self {
		Self::EVMOpcode {
			pc: 0,
			op: EvmOpcodeV1(0),
			stack: Vec::new(),
			memory: Vec::new(),
			storage: None,
		}
	}
}

#[derive(TypeInfo, Encode, Decode, Clone, Debug, Eq, PartialEq)]
pub struct EvmOpcodeV1(pub u8);

impl Serialize for EvmOpcodeV1 {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		OpCode::new(self.0)
			.map(|opcode| opcode.info().name())
			.unwrap_or("INVALID")
			.serialize(serializer)
	}
}

impl<'de> Deserialize<'de> for EvmOpcodeV1 {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let opcode = <&str>::deserialize(deserializer)?;
		OpCode::parse(opcode)
			.ok_or_else(|| serde::de::Error::custom(alloc::format!("Invalid EVM Opcode: {opcode}")))
			.map(|opcode| Self(opcode.get()))
	}
}

#[derive(
	Clone,
	Copy,
	Debug,
	PartialEq,
	Eq,
	PartialOrd,
	Ord,
	Hash,
	Encode,
	Decode,
	TypeInfo,
	Serialize,
	num_enum::IntoPrimitive,
	num_enum::TryFromPrimitive,
	strum::Display,
	strum::EnumString,
	strum::IntoStaticStr,
	strum::VariantArray,
)]
#[serde(into = "&'static str")]
#[strum(serialize_all = "snake_case")]
#[repr(u8)]
pub enum PolkavmSyscallV1 {
	Noop,
	SetStorage,
	SetStorageOrClear,
	GetStorage,
	GetStorageOrZero,
	Call,
	CallEvm,
	DelegateCall,
	DelegateCallEvm,
	Instantiate,
	CallDataSize,
	CallDataCopy,
	CallDataLoad,
	SealReturn,
	Caller,
	Origin,
	CodeHash,
	CodeSize,
	Address,
	GetImmutableData,
	SetImmutableData,
	Balance,
	BalanceOf,
	ChainId,
	GasLimit,
	ValueTransferred,
	GasPrice,
	BaseFee,
	Now,
	DepositEvent,
	BlockNumber,
	BlockHash,
	BlockAuthor,
	#[strum(serialize = "hash_keccak_256")]
	HashKeccak256,
	ReturnDataSize,
	ReturnDataCopy,
	RefTimeLeft,
	ConsumeAllGas,
	EcdsaToEthAddress,
	Sr25519Verify,
	Terminate,
	PvmFuel,
}

impl<'de> Deserialize<'de> for PolkavmSyscallV1 {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		<&str>::deserialize(deserializer)?
			.parse()
			.map_err(|_| serde::de::Error::custom("invalid PolkaVM syscall"))
	}
}
