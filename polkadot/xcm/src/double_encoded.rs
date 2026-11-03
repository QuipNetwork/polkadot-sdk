// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use crate::{DECODE_ALL_ERR_MSG, MAX_XCM_DECODE_DEPTH, RECURSION_LIMIT};
use alloc::vec::Vec;
use codec::{Decode, DecodeLimit, DecodeWithMemTracking, Encode};
use sp_runtime::Saturating;

pub(crate) const DECODE_MAX_DEPTH_MSG: &str =
	"Depth limit exceeded while decoding DoubleEncoded object";
pub(crate) const DECODE_RECURSION_LIMIT_MSG: &str =
	"Recursion limit exceeded while decoding DoubleEncoded object";

environmental::environmental!(nesting_count: u32);

fn descend_ref_and_check_depth(
	depth: &mut u32,
	depth_limit: u32,
	err_msg: &'static str,
) -> Result<(), codec::Error> {
	depth.saturating_inc();
	if *depth > depth_limit {
		return Err(err_msg.into());
	}
	Ok(())
}

/// `Input` implementation used for recursively decoding nested `DoubleEncoded` structures.
///
/// One instance of this input corresponds to one `DoubleEncoded` structure being decoded.
/// For nested `DoubleEncoded` structures, the decoding logic will create an equal number of
/// `NestedInput`s chained through the `downstream_input` field. For example:
/// ```ignore
/// NestedInput {
/// 	downstream_input: NestedInput {
/// 		downstream_input: NestedInput { downstream_input: ... }
/// 	}
/// }
/// ```
///
/// Has the following behaviors:
/// - propagates the memory allocation notifications to the downstream input. This way the
///   downstream input will have a full picture of the entire heap memory used by the top-level
///   decoded object, including the double encoded structures nested within it
/// - doesn't propagate the depth related notifications to the downstream input: acts as if the
///   double encoded structure doesn't add depth to the top-level decoded object
/// - keeps track of and limits the decoding depth of the `DoubleEncoded` structure currently
///   decoded.
struct NestedInput<'a> {
	downstream_input: &'a mut dyn codec::Input,
	encoded: &'a [u8],
	depth: u32,
}

impl<'a> codec::Input for NestedInput<'a> {
	fn remaining_len(&mut self) -> Result<Option<usize>, codec::Error> {
		self.encoded.remaining_len()
	}

	fn read(&mut self, into: &mut [u8]) -> Result<(), codec::Error> {
		self.encoded.read(into)
	}

	fn read_byte(&mut self) -> Result<u8, codec::Error> {
		self.encoded.read_byte()
	}

	fn descend_ref(&mut self) -> Result<(), codec::Error> {
		descend_ref_and_check_depth(&mut self.depth, MAX_XCM_DECODE_DEPTH, DECODE_MAX_DEPTH_MSG)
	}

	fn ascend_ref(&mut self) {
		self.depth.saturating_dec();
	}

	fn on_before_alloc_mem(&mut self, size: usize) -> Result<(), codec::Error> {
		self.downstream_input.on_before_alloc_mem(size)
	}
}

/// Wrapper around the encoded and decoded versions of a value.
/// Caches the decoded value once computed.
#[derive(Encode, DecodeWithMemTracking, scale_info::TypeInfo)]
#[codec(encode_bound())]
#[codec(decode_with_mem_tracking_bound(T: Decode))]
#[scale_info(bounds(), skip_type_params(T))]
#[scale_info(replace_segment("staging_xcm", "xcm"))]
#[cfg_attr(feature = "json-schema", derive(schemars::JsonSchema))]
pub struct DoubleEncoded<T> {
	encoded: Vec<u8>,
	#[codec(skip)]
	decoded: Option<T>,
}

impl<T> Decode for DoubleEncoded<T>
where
	T: Decode,
{
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let mut obj = Self { encoded: Vec::<u8>::decode(input)?, decoded: None };

		// If it's a local call, we also decode the inner double encoded object,
		// in order to make sure that its heap memory is accounted for.
		nesting_count::using_once(&mut 0, || {
			nesting_count::with(|count| {
				descend_ref_and_check_depth(
					count,
					RECURSION_LIMIT as u32,
					DECODE_RECURSION_LIMIT_MSG,
				)
			})
			.unwrap_or(Err("Could not access nesting_count env variable".into()))?;

			let mut nested_input =
				NestedInput { downstream_input: input, encoded: &obj.encoded[..], depth: 0 };
			let decoded = T::decode(&mut nested_input)?;
			// If we didn't manage to consume any byte, this is a remote call, and it can't
			// be decoded locally.
			if nested_input.encoded.len() == obj.encoded.len() {
				let _ = nesting_count::with(|count| {
					count.saturating_dec();
				});

				return Ok(obj);
			}
			obj.decoded = Some(decoded);

			// We need to also make sure that we consumed all the input data, but we can't use
			// `decode_all()`, because it only accepts a byte slice as input.
			if !nested_input.encoded.is_empty() {
				return Err(DECODE_ALL_ERR_MSG.into());
			}

			let _ = nesting_count::with(|count| {
				count.saturating_dec();
			});

			Ok(obj)
		})
	}
}

impl<T> Clone for DoubleEncoded<T> {
	fn clone(&self) -> Self {
		Self { encoded: self.encoded.clone(), decoded: None }
	}
}

impl<T> PartialEq for DoubleEncoded<T> {
	fn eq(&self, other: &Self) -> bool {
		self.encoded.eq(&other.encoded)
	}
}
impl<T> Eq for DoubleEncoded<T> {}

impl<T> core::fmt::Debug for DoubleEncoded<T> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		array_bytes::bytes2hex("0x", &self.encoded).fmt(f)
	}
}

impl<T> From<Vec<u8>> for DoubleEncoded<T> {
	fn from(encoded: Vec<u8>) -> Self {
		Self { encoded, decoded: None }
	}
}

impl<T> DoubleEncoded<T> {
	pub fn encoded(&self) -> &[u8] {
		&self.encoded
	}

	/// Converts a `DoubleEncoded<T>` into a `DoubleEncoded<S>`, dropping the decoded value.
	pub fn transmute_encoded<S>(self) -> DoubleEncoded<S> {
		DoubleEncoded { encoded: self.encoded, decoded: None }
	}
}

impl<T: Decode> DoubleEncoded<T> {
	/// Decode the inner encoded value and store it.
	/// Returns a reference to the value in case of success and `Err(())` in case the decoding
	/// fails.
	pub fn ensure_decoded(&mut self) -> Result<&T, ()> {
		if self.decoded.is_none() {
			self.decoded =
				T::decode_all_with_depth_limit(MAX_XCM_DECODE_DEPTH, &mut &self.encoded[..]).ok();
		}
		self.decoded.as_ref().ok_or(())
	}

	/// Provides an API similar to `TryInto` that allows fallible conversion to the inner value
	/// type. `TryInto` implementation would collide with std blanket implementation based on
	/// `TryFrom`.
	pub fn try_into(mut self) -> Result<T, ()> {
		self.ensure_decoded()?;
		self.decoded.ok_or(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn ensure_decoded_works() {
		let val: u64 = 42;
		let mut encoded: DoubleEncoded<_> = Encode::encode(&val).into();
		assert_eq!(encoded.ensure_decoded(), Ok(&val));
	}

	#[test]
	fn try_into_works() {
		let val: u64 = 42;
		let encoded: DoubleEncoded<_> = Encode::encode(&val).into();
		assert_eq!(encoded.try_into(), Ok(val));
	}
}
