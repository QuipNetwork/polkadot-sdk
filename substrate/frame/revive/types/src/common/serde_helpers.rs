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

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use alloy_core::hex;
use num_traits::Zero;
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeMap};

use crate::common::Bytes;

pub mod hex_serde {
	use super::*;

	pub trait HexCodec: Sized {
		type Error;
		fn to_hex(&self) -> String;
		fn from_hex(s: String) -> Result<Self, Self::Error>;
	}

	macro_rules! impl_hex_codec {
		($($t:ty),*) => {
			$(
				impl HexCodec for $t {
					type Error = core::num::ParseIntError;
					fn to_hex(&self) -> String {
						format!("0x{:x}", self)
					}
					fn from_hex(s: String) -> Result<Self, Self::Error> {
						<$t>::from_str_radix(s.trim_start_matches("0x"), 16)
					}
				}
			)*
		};
	}

	impl_hex_codec!(u8, u32, u64);

	impl<const T: usize> HexCodec for [u8; T] {
		type Error = hex::FromHexError;

		fn to_hex(&self) -> String {
			format!("0x{}", hex::encode(self))
		}

		fn from_hex(s: String) -> Result<Self, Self::Error> {
			let data = hex::decode(s.trim_start_matches("0x"))?;
			data.try_into().map_err(|_| hex::FromHexError::InvalidStringLength)
		}
	}

	impl HexCodec for Vec<u8> {
		type Error = hex::FromHexError;

		fn to_hex(&self) -> String {
			format!("0x{}", hex::encode(self))
		}

		fn from_hex(s: String) -> Result<Self, Self::Error> {
			hex::decode(s.trim_start_matches("0x"))
		}
	}

	pub fn serialize<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
		T: HexCodec,
	{
		let s = value.to_hex();
		serializer.serialize_str(&s)
	}

	pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
	where
		D: Deserializer<'de>,
		T: HexCodec,
		<T as HexCodec>::Error: core::fmt::Debug,
	{
		let s = String::deserialize(deserializer)?;
		let value = T::from_hex(s).map_err(|e| serde::de::Error::custom(format!("{:?}", e)))?;
		Ok(value)
	}

	pub mod option {
		use super::*;

		pub fn serialize<S, T>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: Serializer,
			T: HexCodec,
		{
			match value {
				Some(v) => serializer.serialize_str(&v.to_hex()),
				None => serializer.serialize_none(),
			}
		}

		pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
		where
			D: Deserializer<'de>,
			T: HexCodec,
			<T as HexCodec>::Error: core::fmt::Debug,
		{
			let opt = Option::<String>::deserialize(deserializer)?;
			match opt {
				Some(s) => T::from_hex(s)
					.map(Some)
					.map_err(|e| serde::de::Error::custom(format!("{:?}", e))),
				None => Ok(None),
			}
		}
	}

	pub mod vec {
		use super::*;
		use serde::ser::SerializeSeq;

		pub fn serialize<S, T>(values: &Vec<T>, serializer: S) -> Result<S::Ok, S::Error>
		where
			S: Serializer,
			T: HexCodec,
		{
			let mut seq = serializer.serialize_seq(Some(values.len()))?;
			for v in values {
				seq.serialize_element(&v.to_hex())?;
			}
			seq.end()
		}

		pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
		where
			D: Deserializer<'de>,
			T: HexCodec,
			<T as HexCodec>::Error: core::fmt::Debug,
		{
			let strings = Vec::<String>::deserialize(deserializer)?;
			strings
				.into_iter()
				.map(|s| T::from_hex(s).map_err(|e| serde::de::Error::custom(format!("{:?}", e))))
				.collect()
		}
	}
}

pub fn serialize_stack_minimal<S>(stack: &Vec<Bytes>, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let minimal_values: Vec<String> = stack.iter().map(|bytes| bytes.to_short_hex()).collect();
	minimal_values.serialize(serializer)
}

pub fn deserialize_stack_minimal<'de, D>(deserializer: D) -> Result<Vec<Bytes>, D::Error>
where
	D: Deserializer<'de>,
{
	let strings = Vec::<String>::deserialize(deserializer)?;
	strings
		.into_iter()
		.map(|s| {
			let s = s.trim_start_matches("0x");
			let value = sp_core::U256::from_str_radix(s, 16)
				.map_err(|e| serde::de::Error::custom(alloc::format!("{:?}", e)))?;
			let bytes = value.to_big_endian();
			let trimmed = bytes
				.iter()
				.position(|&b| b != 0)
				.map(|pos| bytes[pos..].to_vec())
				.unwrap_or_else(|| alloc::vec![0u8]);
			Ok(Bytes::from(trimmed))
		})
		.collect()
}

pub fn serialize_memory_no_prefix<S>(memory: &Vec<Bytes>, serializer: S) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	let hex_values: Vec<String> = memory.iter().map(|bytes| bytes.to_hex_no_prefix()).collect();
	hex_values.serialize(serializer)
}

pub fn serialize_storage_no_prefix<S>(
	storage: &Option<BTreeMap<Bytes, Bytes>>,
	serializer: S,
) -> Result<S::Ok, S::Error>
where
	S: Serializer,
{
	match storage {
		None => serializer.serialize_none(),
		Some(map) => {
			let mut ser_map = serializer.serialize_map(Some(map.len()))?;
			for (key, value) in map {
				ser_map.serialize_entry(&key.to_hex_no_prefix(), &value.to_hex_no_prefix())?;
			}
			ser_map.end()
		},
	}
}

pub mod option_value_map {
	use super::*;

	pub fn is_empty<'a, K: 'a, V: 'a>(
		iterator: impl IntoIterator<Item = (&'a K, &'a Option<V>)>,
	) -> bool {
		!iterator.into_iter().any(|(_, value)| value.is_some())
	}

	pub fn serialize_skip_none<'a, K, V, S>(
		iterator: impl IntoIterator<Item = (&'a K, &'a Option<V>)>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		K: Serialize + 'a,
		V: Serialize + 'a,
		S: Serializer,
	{
		serializer.collect_map(iterator.into_iter().filter_map(|(k, v)| v.as_ref().map(|v| (k, v))))
	}
}

pub fn zero_to_none<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
	D: Deserializer<'de>,
	T: Deserialize<'de> + Zero,
{
	let value = Option::<T>::deserialize(deserializer)?;
	match value {
		Some(value) if value.is_zero() => Ok(None),
		Some(value) => Ok(Some(value)),
		None => Ok(None),
	}
}
