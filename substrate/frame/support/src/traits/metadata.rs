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

//! Traits for managing information attached to pallets and their constituents.

use alloc::{vec, vec::Vec};
use codec::{Decode, Encode};
use core::ops::Add;
use impl_trait_for_tuples::impl_for_tuples;
use Debug;

/// Provides information about the pallet itself and its setup in the runtime.
///
/// An implementor should be able to provide information about each pallet that
/// is configured in `construct_runtime!`.
pub trait PalletInfo {
	/// Convert the given pallet `P` into its index as configured in the runtime.
	fn index<P: 'static>() -> Option<usize>;
	/// Convert the given pallet `P` into its name as configured in the runtime.
	fn name<P: 'static>() -> Option<&'static str>;
	/// The two128 hash of name.
	fn name_hash<P: 'static>() -> Option<[u8; 16]>;
	/// Convert the given pallet `P` into its Rust module name as used in `construct_runtime!`.
	fn module_name<P: 'static>() -> Option<&'static str>;
	/// Convert the given pallet `P` into its containing crate version.
	fn crate_version<P: 'static>() -> Option<CrateVersion>;
}

/// Information regarding an instance of a pallet.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct PalletInfoData {
	/// Index of the pallet as configured in the runtime.
	pub index: usize,
	/// Name of the pallet as configured in the runtime.
	pub name: &'static str,
	/// Name of the Rust module containing the pallet.
	pub module_name: &'static str,
	/// Version of the crate containing the pallet.
	pub crate_version: CrateVersion,
}

/// Provides information about the pallet itself and its setup in the runtime.
///
/// Declare some information and access the information provided by [`PalletInfo`] for a specific
/// pallet.
pub trait PalletInfoAccess {
	/// Index of the pallet as configured in the runtime.
	fn index() -> usize;
	/// Name of the pallet as configured in the runtime.
	fn name() -> &'static str;
	/// Two128 hash of name.
	fn name_hash() -> [u8; 16];
	/// Name of the Rust module containing the pallet.
	fn module_name() -> &'static str;
	/// Version of the crate containing the pallet.
	fn crate_version() -> CrateVersion;
}

/// Provide information about a bunch of pallets.
pub trait PalletsInfoAccess {
	/// The number of pallets' information that this type represents.
	///
	/// You probably don't want this function but `infos()` instead.
	fn count() -> usize {
		// for backwards compatibility with XCM-3, Mark as deprecated.
		Self::infos().len()
	}

	/// All of the pallets' information that this type represents.
	fn infos() -> Vec<PalletInfoData>;
}

#[cfg_attr(all(not(feature = "tuples-96"), not(feature = "tuples-128")), impl_for_tuples(64))]
#[cfg_attr(all(feature = "tuples-96", not(feature = "tuples-128")), impl_for_tuples(96))]
#[cfg_attr(feature = "tuples-128", impl_for_tuples(128))]
impl PalletsInfoAccess for Tuple {
	fn infos() -> Vec<PalletInfoData> {
		let mut res = vec![];
		for_tuples!( #( res.extend(Tuple::infos()); )* );
		res
	}
}

/// The function and pallet name of the Call.
#[derive(Clone, Eq, PartialEq, Default, Debug)]
pub struct CallMetadata {
	/// Name of the function.
	pub function_name: &'static str,
	/// Name of the pallet to which the function belongs.
	pub pallet_name: &'static str,
}

/// Gets the function name of the Call.
pub trait GetCallName {
	/// Return all function names in the same order as [`GetCallIndex`].
	fn get_call_names() -> &'static [&'static str];
	/// Return the function name of the Call.
	fn get_call_name(&self) -> &'static str;
}

/// Gets the function index of the Call.
pub trait GetCallIndex {
	/// Return all call indices in the same order as [`GetCallName`].
	fn get_call_indices() -> &'static [u8];
	/// Return the index of this Call.
	fn get_call_index(&self) -> u8;
}

/// Gets the metadata for the Call - function name and pallet name.
pub trait GetCallMetadata {
	/// Return all module names.
	fn get_module_names() -> &'static [&'static str];
	/// Return all function names for the given `module`.
	fn get_call_names(module: &str) -> &'static [&'static str];
	/// Return a [`CallMetadata`], containing function and pallet name of the Call.
	fn get_call_metadata(&self) -> CallMetadata;
}

/// The version of a crate.
#[derive(Debug, Eq, PartialEq, Encode, Decode, Clone, Copy, Default)]
pub struct CrateVersion {
	/// The major version of the crate.
	pub major: u16,
	/// The minor version of the crate.
	pub minor: u8,
	/// The patch version of the crate.
	pub patch: u8,
}

impl CrateVersion {
	pub const fn new(major: u16, minor: u8, patch: u8) -> Self {
		Self { major, minor, patch }
	}
}

impl Ord for CrateVersion {
	fn cmp(&self, other: &Self) -> core::cmp::Ordering {
		self.major
			.cmp(&other.major)
			.then_with(|| self.minor.cmp(&other.minor).then_with(|| self.patch.cmp(&other.patch)))
	}
}

impl PartialOrd for CrateVersion {
	fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
		Some(<Self as Ord>::cmp(self, other))
	}
}

/// The storage key postfix that is used to store the [`StorageVersion`] per pallet.
///
/// The full storage key is built by using:
/// Twox128([`PalletInfo::name`]) ++ Twox128([`STORAGE_VERSION_STORAGE_KEY_POSTFIX`])
pub const STORAGE_VERSION_STORAGE_KEY_POSTFIX: &[u8] = b":__STORAGE_VERSION__:";

/// The storage version of a pallet.
///
/// Each storage version of a pallet is stored in the state under a fixed key. See
/// [`STORAGE_VERSION_STORAGE_KEY_POSTFIX`] for how this key is built.
#[derive(Debug, Eq, PartialEq, Encode, Decode, Ord, Clone, Copy, PartialOrd, Default)]
pub struct StorageVersion(u16);

impl StorageVersion {
	/// Creates a new instance of `Self`.
	pub const fn new(version: u16) -> Self {
		Self(version)
	}

	/// Returns the storage key for a storage version.
	///
	/// See [`STORAGE_VERSION_STORAGE_KEY_POSTFIX`] on how this key is built.
	pub fn storage_key<P: PalletInfoAccess>() -> [u8; 32] {
		let pallet_name = P::name();
		crate::storage::storage_prefix(pallet_name.as_bytes(), STORAGE_VERSION_STORAGE_KEY_POSTFIX)
	}

	/// Put this storage version for the given pallet into the storage.
	///
	/// It will use the storage key that is associated with the given `Pallet`.
	///
	/// # Panics
	///
	/// This function will panic iff `Pallet` can not be found by `PalletInfo`.
	/// In a runtime that is put together using
	/// [`construct_runtime!`](crate::construct_runtime) this should never happen.
	///
	/// It will also panic if this function isn't executed in an externalities
	/// provided environment.
	pub fn put<P: PalletInfoAccess>(&self) {
		let key = Self::storage_key::<P>();

		crate::storage::unhashed::put(&key, self);
	}

	/// Get the storage version of the given pallet from the storage.
	///
	/// It will use the storage key that is associated with the given `Pallet`.
	///
	/// # Panics
	///
	/// This function will panic iff `Pallet` can not be found by `PalletInfo`.
	/// In a runtime that is put together using
	/// [`construct_runtime!`](crate::construct_runtime) this should never happen.
	///
	/// It will also panic if this function isn't executed in an externalities
	/// provided environment.
	pub fn get<P: PalletInfoAccess>() -> Self {
		let key = Self::storage_key::<P>();

		crate::storage::unhashed::get_or_default(&key)
	}

	/// Returns if the storage version key for the given pallet exists in storage.
	///
	/// See [`STORAGE_VERSION_STORAGE_KEY_POSTFIX`] on how this key is built.
	///
	/// # Panics
	///
	/// This function will panic iff `Pallet` can not be found by `PalletInfo`.
	/// In a runtime that is put together using
	/// [`construct_runtime!`](crate::construct_runtime) this should never happen.
	///
	/// It will also panic if this function isn't executed in an externalities
	/// provided environment.
	pub fn exists<P: PalletInfoAccess>() -> bool {
		let key = Self::storage_key::<P>();
		crate::storage::unhashed::exists(&key)
	}
}

impl PartialEq<u16> for StorageVersion {
	fn eq(&self, other: &u16) -> bool {
		self.0 == *other
	}
}

impl PartialOrd<u16> for StorageVersion {
	fn partial_cmp(&self, other: &u16) -> Option<core::cmp::Ordering> {
		Some(self.0.cmp(other))
	}
}

impl Add<u16> for StorageVersion {
	type Output = StorageVersion;

	fn add(self, rhs: u16) -> Self::Output {
		Self::new(self.0 + rhs)
	}
}

/// Special marker struct used when [`storage_version`](crate::pallet_macros::storage_version) is
/// not defined for a pallet.
///
/// If you (the reader) end up here, it probably means that you tried to compare
/// [`GetStorageVersion::on_chain_storage_version`] against
/// [`GetStorageVersion::in_code_storage_version`]. This basically means that the
/// [`storage_version`](crate::pallet_macros::storage_version) is missing from the pallet where the
/// mentioned functions are being called, and needs to be defined.
#[derive(Debug, Default)]
pub struct NoStorageVersionSet;

/// Provides information about a pallet's storage versions.
///
/// Every pallet has two storage versions:
/// 1. An in-code storage version
/// 2. An on-chain storage version
///
/// The in-code storage version is the version of the pallet as defined in the runtime blob, and the
/// on-chain storage version is the version of the pallet stored on-chain.
///
/// Storage versions should be only ever be out of sync when a pallet has been updated to a new
/// version and the in-code version is incremented, but the migration has not yet been executed
/// on-chain as part of a runtime upgrade.
///
/// It is the responsibility of the developer to ensure that the on-chain storage version is set
/// correctly during a migration so that it matches the in-code storage version.
pub trait GetStorageVersion {
	/// This type is generated by the [`pallet`](crate::pallet) macro.
	///
	/// If the [`storage_version`](crate::pallet_macros::storage_version) attribute isn't specified,
	/// this is set to [`NoStorageVersionSet`] to signify that it is missing.
	///
	/// If the [`storage_version`](crate::pallet_macros::storage_version) attribute is specified,
	/// this is be set to a [`StorageVersion`] corresponding to the attribute.
	///
	/// The intention of using [`NoStorageVersionSet`] instead of defaulting to a [`StorageVersion`]
	/// of zero is to prevent developers from forgetting to set
	/// [`storage_version`](crate::pallet_macros::storage_version) when it is required, like in the
	/// case that they wish to compare the in-code storage version to the on-chain storage version.
	type InCodeStorageVersion;

	/// Returns the in-code storage version as specified in the
	/// [`storage_version`](crate::pallet_macros::storage_version) attribute, or
	/// [`NoStorageVersionSet`] if the attribute is missing.
	fn in_code_storage_version() -> Self::InCodeStorageVersion;
	/// Returns the storage version of the pallet as last set in the actual on-chain storage.
	fn on_chain_storage_version() -> StorageVersion;
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_crypto_hashing::twox_128;

	#[allow(dead_code)]
	struct Pallet1;
	impl PalletInfoAccess for Pallet1 {
		fn index() -> usize {
			1
		}
		fn name() -> &'static str {
			"Pallet1"
		}
		fn name_hash() -> [u8; 16] {
			twox_128(Self::name().as_bytes())
		}
		fn module_name() -> &'static str {
			"pallet1"
		}
		fn crate_version() -> CrateVersion {
			CrateVersion::new(1, 0, 0)
		}
	}
	#[allow(dead_code)]
	struct Pallet2;
	impl PalletInfoAccess for Pallet2 {
		fn index() -> usize {
			2
		}
		fn name() -> &'static str {
			"Pallet2"
		}

		fn name_hash() -> [u8; 16] {
			twox_128(Self::name().as_bytes())
		}

		fn module_name() -> &'static str {
			"pallet2"
		}
		fn crate_version() -> CrateVersion {
			CrateVersion::new(1, 0, 0)
		}
	}

	#[test]
	fn check_storage_version_ordering() {
		let version = StorageVersion::new(1);
		assert!(version == StorageVersion::new(1));
		assert!(version < StorageVersion::new(2));
		assert!(version < StorageVersion::new(3));

		let version = StorageVersion::new(2);
		assert!(version < StorageVersion::new(3));
		assert!(version > StorageVersion::new(1));
		assert!(version < StorageVersion::new(5));
	}
}
