// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: MIT-0

// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is furnished to do
// so, subject to the following conditions:

// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! # Single Block Migration Example Pallet
//!
//! An example pallet demonstrating best-practices for writing single-block migrations in the
//! context of upgrading pallet storage.
//!
//! ## Forewarning
//!
//! Single block migrations **MUST** execute in a single block, therefore when executed on a
//! parachain are only appropriate when guaranteed to not exceed block weight limits. If a
//! parachain submits a block that exceeds the block weight limit it will **brick the chain**!
//!
//! If weight is a concern or you are not sure which type of migration to use, you should probably
//! use a multi-block migration.
//!
//! See the [`pallet-example-mbm`](https://paritytech.github.io/polkadot-sdk/master/pallet_example_mbm/index.html)
//! pallet for an example of a multi-block migration.
//!
//! ## Pallet Overview
//!
//! This example pallet contains a single storage item [`Value`](pallet::Value), which may be set by
//! any signed origin by calling the [`set_value`](crate::Call::set_value) extrinsic.
//!
//! For the purposes of this exercise, we imagine that in [`StorageVersion`] V0 of this pallet
//! [`Value`](pallet::Value) is a `u32`, and this what is currently stored on-chain.
//!
//! ```ignore
//! // (Old) Storage Version V0 representation of `Value`
//! #[pallet::storage]
//! pub type Value<T: Config> = StorageValue<_, u32>;
//! ```
//!
//! In [`StorageVersion`] V1 of the pallet a new struct [`CurrentAndPreviousValue`] is introduced:
#![doc = docify::embed!("src/lib.rs", CurrentAndPreviousValue)]
//! and [`Value`](pallet::Value) is updated to store this new struct instead of a `u32`:
#![doc = docify::embed!("src/lib.rs", Value)]
//! In StorageVersion V1 of the pallet when [`set_value`](crate::Call::set_value) is called, the
//! new value is stored in the `current` field of [`CurrentAndPreviousValue`], and the previous
//! value (if it exists) is stored in the `previous` field.
#![doc = docify::embed!("src/lib.rs", pallet_calls)]
//! ## Why a migration is necessary
//!
//! Without a migration, there will be a discrepancy between the on-chain storage for [`Value`] (in
//! V0 it is a `u32`) and the current storage for [`Value`] (in V1 it was changed to a
//! [`CurrentAndPreviousValue`] struct).
//!
//! The on-chain storage for [`Value`] would be a `u32` but the runtime would try to read it as a
//! [`CurrentAndPreviousValue`]. This would result in unacceptable undefined behavior.
//!
//! ## Adding a migration module
//!
//! Writing a pallets migrations in a separate module is strongly recommended.
//!
//! Here's how the migration module is defined for this pallet:
//!
//! ```text
//! substrate/frame/examples/single-block-migrations/src/
//! ├── lib.rs       <-- pallet definition
//! ├── Cargo.toml   <-- pallet manifest
//! └── migrations/
//!    ├── mod.rs    <-- migrations module definition
//!    └── v1.rs     <-- migration logic for the V0 to V1 transition
//! ```
//!
//! This structure allows keeping migration logic separate from the pallet logic and
//! easily adding new migrations in the future.
//!
//! ## Writing the Migration
//!
//! All code related to the migration can be found under
//! [`v1.rs`](migrations::v1).
//!
//! See the migration source code for detailed comments.
//!
//! Here's a brief overview of modules and types defined in `v1.rs`:
//!
//! ### `mod v0`
//!
//! Here we define a [`storage_alias`](frame_support::storage_alias) for the old v0 [`Value`]
//! format.
//!
//! This allows reading the old v0 value from storage during the migration.
//!
//! ### `InnerMigrateV0ToV1`
//!
//! Here we define our raw migration logic,
//! `InnerMigrateV0ToV1` which implements the [`UncheckedOnRuntimeUpgrade`] trait.
//!
//! #### Why [`UncheckedOnRuntimeUpgrade`]?
//!
//! Otherwise, we would have two implementations of [`OnRuntimeUpgrade`] which could be confusing,
//! and may lead to accidentally using the wrong one.
//!
//! #### Standalone Struct or Pallet Hook?
//!
//! Note that the storage migration logic is attached to a standalone struct implementing
//! [`UncheckedOnRuntimeUpgrade`], rather than implementing the
//! [`Hooks::on_runtime_upgrade`](frame_support::traits::Hooks::on_runtime_upgrade) hook directly on
//! the pallet. The pallet hook is better suited for special types of logic that need to execute on
//! every runtime upgrade, but not so much for one-off storage migrations.
//!
//! ### `MigrateV0ToV1`
//!
//! Here, `InnerMigrateV0ToV1` is wrapped in a
//! [`VersionedMigration`] to define
//! [`MigrateV0ToV1`](crate::migrations::v1::MigrateV0ToV1), which may be used
//! in runtimes.
//!
//! Using [`VersionedMigration`] ensures that
//! - The migration only runs once when the on-chain storage version is `0`
//! - The on-chain storage version is updated to `1` after the migration executes
//! - Reads and writes from checking and setting the on-chain storage version are accounted for in
//!   the final [`Weight`](frame_support::weights::Weight)
//!
//! ### `mod test`
//!
//! Here basic unit tests are defined for the migration.
//!
//! When writing migration tests, don't forget to check:
//! - `on_runtime_upgrade` returns the expected weight
//! - `post_upgrade` succeeds when given the bytes returned by `pre_upgrade`
//! - Pallet storage is in the expected state after the migration
//!
//! [`VersionedMigration`]: frame_support::migrations::VersionedMigration
//! [`GetStorageVersion`]: frame_support::traits::GetStorageVersion
//! [`OnRuntimeUpgrade`]: frame_support::traits::OnRuntimeUpgrade
//! [`UncheckedOnRuntimeUpgrade`]: frame_support::traits::UncheckedOnRuntimeUpgrade
//! [`MigrateV0ToV1`]: crate::migrations::v1::MigrateV0ToV1

// We make sure this pallet uses `no_std` for compiling to Wasm.
#![cfg_attr(not(feature = "std"), no_std)]
// allow non-camel-case names for storage version V0 value
#![allow(non_camel_case_types)]

// Re-export pallet items so that they can be accessed from the crate namespace.
pub use pallet::*;

// Export migrations so they may be used in the runtime.
pub mod migrations;
#[doc(hidden)]
mod mock;

extern crate alloc;

use codec::{Decode, Encode, MaxEncodedLen};
use frame_support::traits::StorageVersion;

/// Example struct holding the most recently set [`u32`] and the
/// second most recently set [`u32`] (if one existed).
#[docify::export]
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, scale_info::TypeInfo, MaxEncodedLen)]
pub struct CurrentAndPreviousValue {
	/// The most recently set value.
	pub current: u32,
	/// The previous value, if one existed.
	pub previous: Option<u32>,
}

// Pallet for demonstrating storage migrations.
#[frame_support::pallet(dev_mode)]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	/// Define the current [`StorageVersion`] of the pallet.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {}

	/// [`StorageVersion`] V1 of [`Value`].
	///
	/// Currently used.
	#[docify::export]
	#[pallet::storage]
	pub type Value<T: Config> = StorageValue<_, CurrentAndPreviousValue>;

	#[docify::export(pallet_calls)]
	#[pallet::call]
	impl<T: Config> Pallet<T> {
		pub fn set_value(origin: OriginFor<T>, value: u32) -> DispatchResult {
			ensure_signed(origin)?;

			let previous = Value::<T>::get().map(|v| v.current);
			let new_struct = CurrentAndPreviousValue { current: value, previous };
			<Value<T>>::put(new_struct);

			Ok(())
		}
	}
}
