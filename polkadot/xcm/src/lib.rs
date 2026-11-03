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

//! Cross-Consensus Message format data structures.

// NOTE, this crate is meant to be used in many different environments, notably wasm, but not
// necessarily related to FRAME or even Substrate.
//
// Hence, `no_std` rather than sp-runtime.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use codec::{
	Decode, DecodeLimit, DecodeWithMemTracking, Encode, Error as CodecError, Input, MaxEncodedLen,
	MemTrackingInput,
};
use derive_where::derive_where;
use frame_support::dispatch::GetDispatchInfo;
use scale_info::TypeInfo;

pub mod v3;
pub mod v4;
pub mod v5;

pub mod lts {
	pub use super::v4::*;
}

pub mod latest {
	pub use super::v5::*;
}

mod double_encoded;
pub use double_encoded::DoubleEncoded;

mod utils;

#[cfg(test)]
mod tests;

/// Maximum decoded heap size for an XCM.
pub const MAX_XCM_SIZE: usize = 8 * 1024 * 1024;
/// Maximum nesting level for XCM decoding.
///
/// The `DoubleEncoded<T>` calls found within the XCM instructions are ignored when applying this
/// limit. So from this perspective, they are treated as if they don't have depth.
pub const MAX_XCM_DECODE_DEPTH: u32 = 8;
/// The maximum nesting depth allowed for `DoubleEncoded<T>` calls found within XCM instructions.
///
/// The limit is applied only for nested `DoubleEncoded<T>`
pub const RECURSION_LIMIT: u8 = 10;
/// The maximal number of instructions in an XCM before decoding fails.
///
/// This is a deliberate limit - not a technical one.
pub const MAX_INSTRUCTIONS_TO_DECODE: u8 = 100;

const DECODE_ALL_ERR_MSG: &str = "Input buffer has still data left after decoding!";

/// A version of XCM.
pub type Version = u32;

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Unsupported {}
impl Encode for Unsupported {}
impl Decode for Unsupported {
	fn decode<I: Input>(_: &mut I) -> Result<Self, CodecError> {
		Err("Not decodable".into())
	}
}

/// Attempt to convert `self` into a particular version of itself.
pub trait IntoVersion: Sized {
	/// Consume `self` and return same value expressed in some particular `version` of XCM.
	fn into_version(self, version: Version) -> Result<Self, ()>;

	/// Consume `self` and return same value expressed the latest version of XCM.
	fn into_latest(self) -> Result<Self, ()> {
		self.into_version(latest::VERSION)
	}
}

pub trait TryAs<T> {
	fn try_as(&self) -> Result<&T, ()>;
}

// Macro that generated versioned wrapper types.
// NOTE: converting a v4 type into a versioned type will make it v5.
macro_rules! versioned_type {
	($(#[$attr:meta])* pub enum $n:ident {
		$(#[$index3:meta])+
		V3($v3:ty),
		$(#[$index4:meta])+
		V4($v4:ty),
		$(#[$index5:meta])+
		V5($v5:ty),
	}) => {
		#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
		#[codec(encode_bound())]
		#[codec(decode_bound())]
		#[scale_info(replace_segment("staging_xcm", "xcm"))]
		$(#[$attr])*
		pub enum $n {
			$(#[$index3])*
			V3($v3),
			$(#[$index4])*
			V4($v4),
			$(#[$index5])*
			V5($v5),
		}
		impl $n {
			pub fn try_as<T>(&self) -> Result<&T, ()> where Self: TryAs<T> {
				<Self as TryAs<T>>::try_as(&self)
			}
		}
		impl TryAs<$v3> for $n {
			fn try_as(&self) -> Result<&$v3, ()> {
				match &self {
					Self::V3(ref x) => Ok(x),
					_ => Err(()),
				}
			}
		}
		impl TryAs<$v4> for $n {
			fn try_as(&self) -> Result<&$v4, ()> {
				match &self {
					Self::V4(ref x) => Ok(x),
					_ => Err(()),
				}
			}
		}
		impl TryAs<$v5> for $n {
			fn try_as(&self) -> Result<&$v5, ()> {
				match &self {
					Self::V5(ref x) => Ok(x),
					_ => Err(()),
				}
			}
		}
		impl IntoVersion for $n {
			fn into_version(self, n: Version) -> Result<Self, ()> {
				let version = self.identify_version();
				if version == n {
					Ok(self)
				} else {
					Ok(match n {
						3 => Self::V3(self.try_into()?),
						4 => Self::V4(self.try_into()?),
						5 => Self::V5(self.try_into()?),
						_ => return Err(()),
					})
				}
			}
		}
		impl From<$v3> for $n {
			fn from(x: $v3) -> Self {
				$n::V3(x.into())
			}
		}
		impl<T: Into<$v5>> From<T> for $n {
			fn from(x: T) -> Self {
				$n::V5(x.into())
			}
		}
		impl TryFrom<$n> for $v3 {
			type Error = ();
			fn try_from(x: $n) -> Result<Self, ()> {
				use $n::*;
				match x {
					V3(x) => Ok(x),
					V4(x) => x.try_into().map_err(|_| ()),
					V5(x) => {
						let v4: $v4 = x.try_into().map_err(|_| ())?;
						v4.try_into().map_err(|_| ())
					}
				}
			}
		}
		impl TryFrom<$n> for $v4 {
			type Error = ();
			fn try_from(x: $n) -> Result<Self, ()> {
				use $n::*;
				match x {
					V3(x) => x.try_into().map_err(|_| ()),
					V4(x) => Ok(x),
					V5(x) => x.try_into().map_err(|_| ()),
				}
			}
		}
		impl TryFrom<$n> for $v5 {
			type Error = ();
			fn try_from(x: $n) -> Result<Self, ()> {
				use $n::*;
				match x {
					V3(x) => {
						let v4: $v4 = x.try_into().map_err(|_| ())?;
						v4.try_into().map_err(|_| ())
					},
					V4(x) => x.try_into().map_err(|_| ()),
					V5(x) => Ok(x),
				}
			}
		}
		impl MaxEncodedLen for $n {
			fn max_encoded_len() -> usize {
				<$v3>::max_encoded_len()
			}
		}
		impl IdentifyVersion for $n {
			fn identify_version(&self) -> Version {
				use $n::*;
				match self {
					V3(_) => v3::VERSION,
					V4(_) => v4::VERSION,
					V5(_) => v5::VERSION,
				}
			}
		}
	};
}

versioned_type! {
	/// A single version's `AssetId` value, together with its version code.
	pub enum VersionedAssetId {
		#[codec(index = 3)]
		V3(v3::AssetId),
		#[codec(index = 4)]
		V4(v4::AssetId),
		#[codec(index = 5)]
		V5(v5::AssetId),
	}
}

versioned_type! {
	/// A single version's `Response` value, together with its version code.
	pub enum VersionedResponse {
		#[codec(index = 3)]
		V3(v3::Response),
		#[codec(index = 4)]
		V4(v4::Response),
		#[codec(index = 5)]
		V5(v5::Response),
	}
}

versioned_type! {
	/// A single `NetworkId` value, together with its version code.
	pub enum VersionedNetworkId {
		#[codec(index = 3)]
		V3(v3::NetworkId),
		#[codec(index = 4)]
		V4(v4::NetworkId),
		#[codec(index = 5)]
		V5(v5::NetworkId),
	}
}

versioned_type! {
	/// A single `Junction` value, together with its version code.
	pub enum VersionedJunction {
		#[codec(index = 3)]
		V3(v3::Junction),
		#[codec(index = 4)]
		V4(v4::Junction),
		#[codec(index = 5)]
		V5(v5::Junction),
	}
}

versioned_type! {
	/// A single `Location` value, together with its version code.
	#[derive(Ord, PartialOrd)]
	pub enum VersionedLocation {
		#[codec(index = 3)]
		V3(v3::MultiLocation),
		#[codec(index = 4)]
		V4(v4::Location),
		#[codec(index = 5)]
		V5(v5::Location),
	}
}

versioned_type! {
	/// A single `InteriorLocation` value, together with its version code.
	pub enum VersionedInteriorLocation {
		#[codec(index = 3)]
		V3(v3::InteriorMultiLocation),
		#[codec(index = 4)]
		V4(v4::InteriorLocation),
		#[codec(index = 5)]
		V5(v5::InteriorLocation),
	}
}

versioned_type! {
	/// A single `Asset` value, together with its version code.
	pub enum VersionedAsset {
		#[codec(index = 3)]
		V3(v3::MultiAsset),
		#[codec(index = 4)]
		V4(v4::Asset),
		#[codec(index = 5)]
		V5(v5::Asset),
	}
}

versioned_type! {
	/// A single `MultiAssets` value, together with its version code.
	pub enum VersionedAssets {
		#[codec(index = 3)]
		V3(v3::MultiAssets),
		#[codec(index = 4)]
		V4(v4::Assets),
		#[codec(index = 5)]
		V5(v5::Assets),
	}
}

impl VersionedAssets {
	/// The number of assets in the collection, regardless of XCM version.
	pub fn len(&self) -> usize {
		match self {
			Self::V3(assets) => assets.len(),
			Self::V4(assets) => assets.len(),
			Self::V5(assets) => assets.len(),
		}
	}

	/// Whether the collection contains no assets.
	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}
}

/// A single XCM message, together with its version code.
#[derive(Encode, Decode, DecodeWithMemTracking, TypeInfo)]
#[derive_where(Clone, Eq, PartialEq, Debug)]
#[codec(encode_bound())]
#[codec(decode_bound(RuntimeCall: Decode))]
#[codec(decode_with_mem_tracking_bound(RuntimeCall: Decode))]
#[scale_info(bounds(), skip_type_params(RuntimeCall))]
#[scale_info(replace_segment("staging_xcm", "xcm"))]
pub enum VersionedXcm<RuntimeCall> {
	#[codec(index = 3)]
	V3(v3::Xcm<RuntimeCall>),
	#[codec(index = 4)]
	V4(v4::Xcm<RuntimeCall>),
	#[codec(index = 5)]
	V5(v5::Xcm<RuntimeCall>),
}

impl<C: Decode + GetDispatchInfo> IntoVersion for VersionedXcm<C> {
	fn into_version(self, n: Version) -> Result<Self, ()> {
		Ok(match n {
			3 => Self::V3(self.try_into()?),
			4 => Self::V4(self.try_into()?),
			5 => Self::V5(self.try_into()?),
			_ => return Err(()),
		})
	}
}

impl<C> IdentifyVersion for VersionedXcm<C> {
	fn identify_version(&self) -> Version {
		match self {
			Self::V3(_) => v3::VERSION,
			Self::V4(_) => v4::VERSION,
			Self::V5(_) => v5::VERSION,
		}
	}
}

impl<C: Decode> VersionedXcm<C> {
	/// Decodes an XCM, checking the [`MAX_XCM_SIZE`], [`MAX_XCM_DECODE_DEPTH`], and also that all
	/// the input data is consumed.
	///
	/// The implicit constraints baked into the XCM decoding logic (e.g. `MAX_ITEMS_IN_ASSETS` and
	/// [`MAX_INSTRUCTIONS_TO_DECODE`]) are also checked.
	pub fn decode_all_with_mem_and_depth_limit(
		input: &mut &[u8],
	) -> Result<VersionedXcm<C>, CodecError> {
		// Adds 1 byte to the `MAX_XCM_SIZE` as the decoding fails exactly at the given value and
		// the maximum should be allowed to fit in.
		let mut mem_tracking_input = MemTrackingInput::new(input, MAX_XCM_SIZE.saturating_add(1));
		let xcm =
			VersionedXcm::decode_with_depth_limit(MAX_XCM_DECODE_DEPTH, &mut mem_tracking_input)?;
		// We need to also make sure that we consumed all the input data, but we can't use
		// `decode_all()`, because it only accepts a byte slice as input.
		if !input.is_empty() {
			return Err(DECODE_ALL_ERR_MSG.into());
		}

		Ok(xcm)
	}

	/// Checks if the XCM is decodable. Consequently, it checks all decoding constraints,
	/// such as [`MAX_XCM_DECODE_DEPTH`], [`MAX_XCM_SIZE`], `MAX_ITEMS_IN_ASSETS` or
	/// [`MAX_INSTRUCTIONS_TO_DECODE`].
	///
	/// Note that this is a best effort and it has limitations. For example:
	/// - this uses the limit of the sender - not the receiver
	/// - if the XCM contains double encoded calls to be executed on the remote chain, they won't be
	///   decoded here. So the `RECURSION_LIMIT` will not be checked for them, and also their
	///   decoded heap memory will not be included when checking the `MAX_XCM_SIZE`.
	pub fn check_is_decodable(&self) -> Result<(), ()> {
		self.using_encoded(|mut enc| {
			Self::decode_all_with_mem_and_depth_limit(&mut enc).map(|_| ())
		})
		.map_err(|e| {
			tracing::error!(target: "xcm::check_is_decodable", error=?e, xcm=?self, "Decode error!");
			()
		})
	}
}

impl<RuntimeCall> From<v3::Xcm<RuntimeCall>> for VersionedXcm<RuntimeCall> {
	fn from(x: v3::Xcm<RuntimeCall>) -> Self {
		VersionedXcm::V3(x)
	}
}

impl<RuntimeCall> From<v4::Xcm<RuntimeCall>> for VersionedXcm<RuntimeCall> {
	fn from(x: v4::Xcm<RuntimeCall>) -> Self {
		VersionedXcm::V4(x)
	}
}

impl<RuntimeCall> From<v5::Xcm<RuntimeCall>> for VersionedXcm<RuntimeCall> {
	fn from(x: v5::Xcm<RuntimeCall>) -> Self {
		VersionedXcm::V5(x)
	}
}

impl<Call: Decode + GetDispatchInfo> TryFrom<VersionedXcm<Call>> for v3::Xcm<Call> {
	type Error = ();
	fn try_from(x: VersionedXcm<Call>) -> Result<Self, ()> {
		use VersionedXcm::*;
		match x {
			V3(x) => Ok(x),
			V4(x) => x.try_into(),
			V5(x) => {
				let v4: v4::Xcm<Call> = x.try_into()?;
				v4.try_into()
			},
		}
	}
}

impl<Call: Decode + GetDispatchInfo> TryFrom<VersionedXcm<Call>> for v4::Xcm<Call> {
	type Error = ();
	fn try_from(x: VersionedXcm<Call>) -> Result<Self, ()> {
		use VersionedXcm::*;
		match x {
			V3(x) => x.try_into(),
			V4(x) => Ok(x),
			V5(x) => x.try_into(),
		}
	}
}

impl<Call: Decode + GetDispatchInfo> TryFrom<VersionedXcm<Call>> for v5::Xcm<Call> {
	type Error = ();
	fn try_from(x: VersionedXcm<Call>) -> Result<Self, ()> {
		use VersionedXcm::*;
		match x {
			V3(x) => {
				let v4: v4::Xcm<Call> = x.try_into()?;
				v4.try_into()
			},
			V4(x) => x.try_into(),
			V5(x) => Ok(x),
		}
	}
}

/// Convert an `Xcm` datum into a `VersionedXcm`, based on a destination `Location` which will
/// interpret it.
pub trait WrapVersion {
	fn wrap_version<RuntimeCall: Decode + GetDispatchInfo>(
		dest: &latest::Location,
		xcm: impl Into<VersionedXcm<RuntimeCall>>,
	) -> Result<VersionedXcm<RuntimeCall>, ()>;
}

/// Used to get the version out of a versioned type.
// TODO(XCMv5): This could be `GetVersion` and we change the current one to `GetVersionFor`.
pub trait IdentifyVersion {
	fn identify_version(&self) -> Version;
}

/// Check and return the `Version` that should be used for the `Xcm` datum for the destination
/// `Location`, which will interpret it.
pub trait GetVersion {
	fn get_version_for(dest: &latest::Location) -> Option<Version>;
}

/// `()` implementation does nothing with the XCM, just sending with whatever version it was
/// authored as.
impl WrapVersion for () {
	fn wrap_version<RuntimeCall>(
		_: &latest::Location,
		xcm: impl Into<VersionedXcm<RuntimeCall>>,
	) -> Result<VersionedXcm<RuntimeCall>, ()> {
		Ok(xcm.into())
	}
}

/// `WrapVersion` implementation which attempts to always convert the XCM to version 3 before
/// wrapping it.
pub struct AlwaysV3;
impl WrapVersion for AlwaysV3 {
	fn wrap_version<Call: Decode + GetDispatchInfo>(
		_: &latest::Location,
		xcm: impl Into<VersionedXcm<Call>>,
	) -> Result<VersionedXcm<Call>, ()> {
		Ok(VersionedXcm::<Call>::V3(xcm.into().try_into()?))
	}
}
impl GetVersion for AlwaysV3 {
	fn get_version_for(_dest: &latest::Location) -> Option<Version> {
		Some(v3::VERSION)
	}
}

/// `WrapVersion` implementation which attempts to always convert the XCM to version 4 before
/// wrapping it.
pub struct AlwaysV4;
impl WrapVersion for AlwaysV4 {
	fn wrap_version<Call: Decode + GetDispatchInfo>(
		_: &latest::Location,
		xcm: impl Into<VersionedXcm<Call>>,
	) -> Result<VersionedXcm<Call>, ()> {
		Ok(VersionedXcm::<Call>::V4(xcm.into().try_into()?))
	}
}
impl GetVersion for AlwaysV4 {
	fn get_version_for(_dest: &latest::Location) -> Option<Version> {
		Some(v4::VERSION)
	}
}

/// `WrapVersion` implementation which attempts to always convert the XCM to version 5 before
/// wrapping it.
pub struct AlwaysV5;
impl WrapVersion for AlwaysV5 {
	fn wrap_version<Call: Decode + GetDispatchInfo>(
		_: &latest::Location,
		xcm: impl Into<VersionedXcm<Call>>,
	) -> Result<VersionedXcm<Call>, ()> {
		Ok(VersionedXcm::<Call>::V5(xcm.into().try_into()?))
	}
}
impl GetVersion for AlwaysV5 {
	fn get_version_for(_dest: &latest::Location) -> Option<Version> {
		Some(v5::VERSION)
	}
}

/// `WrapVersion` implementation which attempts to always convert the XCM to the latest version
/// before wrapping it.
pub type AlwaysLatest = AlwaysV5;

/// `WrapVersion` implementation which attempts to always convert the XCM to the most recent Long-
/// Term-Support version before wrapping it.
pub type AlwaysLts = AlwaysV4;

pub mod prelude {
	pub use super::{
		latest::prelude::*, AlwaysLatest, AlwaysLts, AlwaysV3, AlwaysV4, AlwaysV5, GetVersion,
		IdentifyVersion, IntoVersion, Unsupported, Version as XcmVersion, VersionedAsset,
		VersionedAssetId, VersionedAssets, VersionedInteriorLocation, VersionedLocation,
		VersionedResponse, VersionedXcm, WrapVersion,
	};

	/// The minimal supported XCM version
	pub const MIN_XCM_VERSION: XcmVersion = 3;
}

pub mod opaque {
	pub mod v3 {
		// Everything from v3
		pub use crate::v3::*;
		// Then override with the opaque types in v3
		pub use crate::v3::opaque::{Instruction, Xcm};
	}
	pub mod v4 {
		// Everything from v4
		pub use crate::v4::*;
		// Then override with the opaque types in v4
		pub use crate::v4::opaque::{Instruction, Xcm};
	}
	pub mod v5 {
		// Everything from v4
		pub use crate::v5::*;
		// Then override with the opaque types in v5
		pub use crate::v5::opaque::{Instruction, Xcm};
	}

	pub mod latest {
		pub use super::v5::*;
	}

	pub mod lts {
		pub use super::v4::*;
	}

	/// The basic `VersionedXcm` type which just uses the `Vec<u8>` as an encoded call.
	pub type VersionedXcm = super::VersionedXcm<()>;
}

#[test]
fn conversion_works() {
	use latest::prelude::*;
	let assets: Assets = (Here, 1u128).into();
	let _: VersionedAssets = assets.into();
}

#[test]
fn size_limits() {
	extern crate std;

	let mut test_failed = false;
	macro_rules! check_sizes {
        ($(($kind:ty, $expected:expr),)+) => {
            $({
                let s = core::mem::size_of::<$kind>();
                // Since the types often affect the size of other types in which they're included
                // it is more convenient to check multiple types at the same time and only fail
                // the test at the end. For debugging it's also useful to print out all of the sizes,
                // even if they're within the expected range.
                if s > $expected {
                    test_failed = true;
                    std::eprintln!(
                        "assertion failed: size of '{}' is {} (which is more than the expected {})",
                        stringify!($kind),
                        s,
                        $expected
                    );
                } else {
                    std::println!(
                        "type '{}' is of size {} which is within the expected {}",
                        stringify!($kind),
                        s,
                        $expected
                    );
                }
            })+
        }
    }

	check_sizes! {
		(crate::latest::Instruction<()>, 128),
		(crate::latest::Asset, 80),
		(crate::latest::Location, 24),
		(crate::latest::AssetId, 40),
		(crate::latest::Junctions, 16),
		(crate::latest::Junction, 88),
		(crate::latest::Response, 40),
		(crate::latest::AssetInstance, 48),
		(crate::latest::NetworkId, 48),
		(crate::latest::BodyId, 32),
		(crate::latest::Assets, 24),
		(crate::latest::BodyPart, 12),
	}
	assert!(!test_failed);
}

#[test]
fn check_is_decodable_works() {
	use crate::{
		latest::{
			prelude::{GeneralIndex, ReserveAssetDeposited, SetAppendix},
			Assets, Xcm, MAX_ITEMS_IN_ASSETS,
		},
		MAX_INSTRUCTIONS_TO_DECODE,
	};

	// closure generates assets of `count`
	let assets = |count| {
		let mut assets = Assets::new();
		for i in 0..count {
			assets.push((GeneralIndex(i as u128), 100).into());
		}
		assets
	};

	// closer generates `Xcm` with nested instructions of `depth`
	let with_instr = |depth| {
		let mut xcm = Xcm::<()>(vec![]);
		for _ in 0..depth - 1 {
			xcm = Xcm::<()>(vec![SetAppendix(xcm)]);
		}
		xcm
	};

	// `MAX_INSTRUCTIONS_TO_DECODE` check
	assert!(VersionedXcm::<()>::from(Xcm(vec![
		ReserveAssetDeposited(assets(1));
		(MAX_INSTRUCTIONS_TO_DECODE - 1) as usize
	]))
	.check_is_decodable()
	.is_ok());
	assert!(VersionedXcm::<()>::from(Xcm(vec![
		ReserveAssetDeposited(assets(1));
		MAX_INSTRUCTIONS_TO_DECODE as usize
	]))
	.check_is_decodable()
	.is_ok());
	assert!(VersionedXcm::<()>::from(Xcm(vec![
		ReserveAssetDeposited(assets(1));
		(MAX_INSTRUCTIONS_TO_DECODE + 1) as usize
	]))
	.check_is_decodable()
	.is_err());

	// `MAX_XCM_DECODE_DEPTH` check
	assert!(VersionedXcm::<()>::from(with_instr(MAX_XCM_DECODE_DEPTH - 1))
		.check_is_decodable()
		.is_ok());
	assert!(VersionedXcm::<()>::from(with_instr(MAX_XCM_DECODE_DEPTH))
		.check_is_decodable()
		.is_ok());
	assert!(VersionedXcm::<()>::from(with_instr(MAX_XCM_DECODE_DEPTH + 1))
		.check_is_decodable()
		.is_err());

	// `MAX_ITEMS_IN_ASSETS` check
	assert!(VersionedXcm::<()>::from(Xcm(vec![ReserveAssetDeposited(assets(
		MAX_ITEMS_IN_ASSETS
	))]))
	.check_is_decodable()
	.is_ok());
	assert!(VersionedXcm::<()>::from(Xcm(vec![ReserveAssetDeposited(assets(
		MAX_ITEMS_IN_ASSETS - 1
	))]))
	.check_is_decodable()
	.is_ok());
	assert!(VersionedXcm::<()>::from(Xcm(vec![ReserveAssetDeposited(assets(
		MAX_ITEMS_IN_ASSETS + 1
	))]))
	.check_is_decodable()
	.is_err());
}
