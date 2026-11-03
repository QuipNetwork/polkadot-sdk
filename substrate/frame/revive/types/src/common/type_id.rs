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
//! Ethereum Typed Transaction types
use crate::common::Byte;
use codec::{Decode, DecodeWithMemTracking, Encode};
use paste::paste;
use rlp::Decodable;
use scale_info::TypeInfo;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A macro to generate Transaction type identifiers
/// See <https://ethereum.org/en/developers/docs/transactions/#typed-transaction-envelope>
macro_rules! transaction_type {
	($name:ident, $value:literal) => {
		#[doc = concat!("Transaction type identifier: ", $value)]
		#[derive(Clone, Default, Debug, Eq, PartialEq, DecodeWithMemTracking)]
		pub struct $name;

		// upper case const name
		paste! {
			#[doc = concat!("Transaction value for type identifier: ", $value)]
			pub const [<$name:snake:upper>]: u8 = $value;
		}

		impl $name {
			/// Convert to u8
			pub fn value(&self) -> u8 {
				$value
			}

			/// Convert to Byte
			pub fn as_byte(&self) -> Byte {
				Byte::from($value)
			}

			/// Try to convert from Byte
			pub fn try_from_byte(byte: Byte) -> Result<Self, Byte> {
				if byte.0 == $value { Ok(Self {}) } else { Err(byte) }
			}
		}

		impl Decodable for $name {
			fn decode(rlp: &rlp::Rlp) -> Result<Self, rlp::DecoderError> {
				let value: u8 = rlp.as_val()?;
				if value == $value {
					Ok(Self {})
				} else {
					Err(rlp::DecoderError::Custom(concat!("expected ", $value)))
				}
			}
		}

		impl Encode for $name {
			fn using_encoded<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
				f(&[$value])
			}
		}
		impl Decode for $name {
			fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
				if $value == input.read_byte()? {
					Ok(Self {})
				} else {
					Err(codec::Error::from(concat!("expected ", $value)))
				}
			}
		}

		impl TypeInfo for $name {
			type Identity = u8;
			fn type_info() -> scale_info::Type {
				<u8 as TypeInfo>::type_info()
			}
		}

		impl Serialize for $name {
			fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
			where
				S: Serializer,
			{
				serializer.serialize_str(concat!("0x", $value))
			}
		}
		impl<'de> Deserialize<'de> for $name {
			fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
			where
				D: Deserializer<'de>,
			{
				let s: &str = Deserialize::deserialize(deserializer)?;
				if s == concat!("0x", $value) {
					Ok($name {})
				} else {
					Err(serde::de::Error::custom(concat!("expected ", $value)))
				}
			}
		}
	};
}

transaction_type!(TypeLegacy, 0);
transaction_type!(TypeEip2930, 1);
transaction_type!(TypeEip1559, 2);
transaction_type!(TypeEip4844, 3);
transaction_type!(TypeEip7702, 4);

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn transaction_type() {
		assert_eq!(TYPE_EIP2930, 1u8);
	}
}
