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

use crate::common::hex_serde::HexCodec;
use alloc::{vec, vec::Vec};
use alloy_core::hex;
use codec::{Decode, DecodeWithMemTracking, Encode};
use core::{
	fmt::{Debug, Display, Formatter, Result as FmtResult},
	str::FromStr,
};
use scale_info::TypeInfo;
use serde::{Deserialize, Serialize};

impl FromStr for Bytes {
	type Err = hex::FromHexError;
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let data = hex::decode(s.trim_start_matches("0x"))?;
		Ok(Bytes(data))
	}
}

macro_rules! impl_hex {
    ($type:ident, $inner:ty, $default:expr) => {
        #[derive(Encode, Decode, Eq, PartialEq, Ord, PartialOrd, TypeInfo, Clone, Serialize, Deserialize, Hash, DecodeWithMemTracking)]
        #[doc = concat!("`", stringify!($inner), "`", " wrapper type for encoding and decoding hex strings")]
        pub struct $type(#[serde(with = "crate::common::hex_serde")] pub $inner);

        impl Default for $type {
            fn default() -> Self {
                $type($default)
            }
        }

        impl From<$inner> for $type {
            fn from(inner: $inner) -> Self {
                $type(inner)
            }
        }

        impl Debug for $type {
            fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
				let hex_str = self.0.to_hex();
				let truncated = &hex_str[..hex_str.len().min(100)];
				let ellipsis = if hex_str.len() > 100 { "..." } else { "" };
                write!(f, concat!(stringify!($type), "({}{})"), truncated,ellipsis)
            }
        }

        impl Display for $type {
            fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
                write!(f, "{}", self.0.to_hex())
            }
        }
    };
}

impl Bytes {
	/// See `Vec::is_empty`
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	/// Convert to minimal hex format without padding 0s
	pub fn to_short_hex(&self) -> alloc::string::String {
		let word = sp_core::U256::from_big_endian(&self.0);
		alloc::format!("0x{:x}", word)
	}

	/// Convert to hex format without "0x" prefix
	pub fn to_hex_no_prefix(&self) -> alloc::string::String {
		alloy_core::hex::encode(&self.0)
	}
}

impl_hex!(Byte, u8, 0u8);
impl_hex!(Bytes, Vec<u8>, vec![]);
impl_hex!(Bytes8, [u8; 8], [0u8; 8]);
impl_hex!(Bytes32, [u8; 32], [0u8; 32]);
impl_hex!(Bytes256, [u8; 256], [0u8; 256]);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_to_short_hex() {
		let bytes = Bytes(sp_core::U256::from(4).to_big_endian().to_vec());
		assert_eq!(bytes.to_short_hex(), "0x4");
	}

	#[test]
	fn test_to_hex_no_prefix() {
		let bytes = Bytes(vec![0x12, 0x34, 0x56, 0x78]);
		assert_eq!(bytes.to_hex_no_prefix(), "12345678");
	}

	#[test]
	fn serialize_works() {
		let a = Byte(42);
		let s = serde_json::to_string(&a).unwrap();
		assert_eq!(s, "\"0x2a\"");
		let b = serde_json::from_str::<Byte>(&s).unwrap();
		assert_eq!(a, b);

		let a = Bytes(b"bello world".to_vec());
		let s = serde_json::to_string(&a).unwrap();
		assert_eq!(s, "\"0x62656c6c6f20776f726c64\"");
		let b = serde_json::from_str::<Bytes>(&s).unwrap();
		assert_eq!(a, b);

		let a = Bytes256([42u8; 256]);
		let s = serde_json::to_string(&a).unwrap();
		let b = serde_json::from_str::<Bytes256>(&s).unwrap();
		assert_eq!(a, b);
	}
}
