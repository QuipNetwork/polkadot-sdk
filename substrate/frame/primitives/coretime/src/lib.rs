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

//! Primitives for Polkadot Coretime.
//!
//! Contains fundamental types and the [`market::Market`] trait used by both `pallet-broker` and
//! market implementations.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod core_mask;
pub mod market;

pub use core_mask::*;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;

/// Index of a Polkadot Core.
pub type CoreIndex = u16;

/// A Coretime timeslice index. Each timeslice represents a fixed block interval.
pub type Timeslice = u32;

/// A Task Id. In general this is called a ParachainId.
pub type TaskId = u32;

/// Fraction expressed as a numerator with an implicit denominator of 57,600.
pub type PartsOf57600 = u16;

/// Self-describing identity for a Region of Bulk Coretime.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct RegionId {
	/// The timeslice at which this Region begins.
	pub begin: Timeslice,
	/// The index of the Polkadot Core on which this Region will be scheduled.
	pub core: CoreIndex,
	/// The regularity parts in which this Region will be scheduled.
	pub mask: CoreMask,
}
impl From<u128> for RegionId {
	fn from(x: u128) -> Self {
		Self { begin: (x >> 96) as u32, core: (x >> 80) as u16, mask: x.into() }
	}
}
impl From<RegionId> for u128 {
	fn from(x: RegionId) -> Self {
		((x.begin as u128) << 96) | ((x.core as u128) << 80) | u128::from(x.mask)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn region_id_converts_u128() {
		let r =
			RegionId { begin: 0x12345678u32, core: 0xabcdu16, mask: 0xdeadbeefcafef00d0123.into() };
		let u = 0x12345678_abcd_deadbeefcafef00d0123u128;
		assert_eq!(RegionId::from(u), r);
		assert_eq!(u128::from(r), u);
	}
}

/// The identity of a possibly renewable Core workload.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct PotentialRenewalId {
	/// The core whose workload at the sale ending with `when` may be renewed to begin at `when`.
	pub core: CoreIndex,
	/// The point in time that the renewable workload on `core` ends and a fresh renewal may begin.
	pub when: Timeslice,
}
