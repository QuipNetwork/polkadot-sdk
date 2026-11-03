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

//! Support code for the runtime.
//!
//! ## Note on Tuple Traits
//!
//! Many of the traits defined in [`traits`] have auto-implementations on tuples as well. Usually,
//! the tuple is a function of number of pallets in the runtime. By default, the traits are
//! implemented for tuples of up to 64 items.
// If you have more pallets in your runtime, or for any other reason need more, enabled `tuples-96`
// or the `tuples-128` complication flag. Note that these features *will increase* the compilation
// of this crate.

#![cfg_attr(not(feature = "std"), no_std)]

/// Export ourself as `frame_support` to make tests happy.
#[doc(hidden)]
extern crate self as frame_support;

#[doc(hidden)]
extern crate alloc;

/// Maximum nesting level for extrinsics.
pub const MAX_EXTRINSIC_DEPTH: u32 = 256;

/// Private exports that are being used by macros.
///
/// The exports are not stable and should not be relied on.
#[doc(hidden)]
pub mod __private {
	pub use alloc::{
		boxed::Box,
		fmt::Debug,
		rc::Rc,
		string::String,
		vec,
		vec::{IntoIter, Vec},
	};
	pub use codec;
	pub use frame_metadata as metadata;
	pub use impl_trait_for_tuples;
	pub use log;
	pub use paste;
	pub use scale_info;
	pub use serde;
	pub use serde_json;
	pub use sp_core::{Get, OpaqueMetadata, Void};
	pub use sp_crypto_hashing_proc_macro;
	pub use sp_inherents;
	#[cfg(feature = "std")]
	pub use sp_io::TestExternalities;
	pub use sp_io::{self, hashing, storage::root as storage_root};
	pub use sp_metadata_ir as metadata_ir;
	#[cfg(feature = "std")]
	pub use sp_runtime::{bounded_btree_map, bounded_vec};
	pub use sp_runtime::{
		traits::{AsSystemOriginSigner, AsTransactionAuthorizedOrigin, Dispatchable},
		DispatchError, StateVersion, TransactionOutcome,
	};
	#[cfg(feature = "std")]
	pub use sp_state_machine::BasicExternalities;
	pub use sp_std;
	pub use sp_tracing;
	pub use tt_call::*;
}

#[macro_use]
pub mod dispatch;
pub mod crypto;
pub mod dispatch_context;
mod hash;
pub mod inherent;
pub mod instances;
mod macros;
pub mod migrations;
pub mod storage;
#[cfg(test)]
mod tests;
pub mod traits;
pub mod view_functions;
pub mod weights;
#[doc(hidden)]
pub mod unsigned {
	#[allow(deprecated)]
	#[doc(hidden)]
	pub use crate::sp_runtime::traits::ValidateUnsigned;
	#[doc(hidden)]
	pub use crate::sp_runtime::transaction_validity::{
		TransactionSource, TransactionValidity, TransactionValidityError, UnknownTransaction,
	};
}

#[cfg(any(feature = "std", feature = "runtime-benchmarks", feature = "try-runtime", test))]
pub use self::storage::storage_noop_guard::StorageNoopGuard;
pub use self::{
	dispatch::{Callable, Parameter},
	hash::{
		Blake2_128, Blake2_128Concat, Blake2_256, Hashable, Identity, ReversibleStorageHasher,
		StorageHasher, Twox128, Twox256, Twox64Concat,
	},
	storage::{
		bounded_btree_map::BoundedBTreeMap,
		bounded_btree_set::BoundedBTreeSet,
		bounded_vec::{BoundedSlice, BoundedVec},
		migration,
		weak_bounded_vec::WeakBoundedVec,
		IterableStorageDoubleMap, IterableStorageMap, IterableStorageNMap, StorageDoubleMap,
		StorageMap, StorageNMap, StoragePrefixedMap, StorageValue,
	},
};
pub use sp_runtime::{
	self, print, traits::Printable, ConsensusEngineId, MAX_MODULE_ERROR_ENCODED_SIZE,
};

use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_runtime::TypeId;

/// A unified log target for support operations.
pub const LOG_TARGET: &str = "runtime::frame-support";

/// A type that cannot be instantiated.
#[derive(Encode, Decode, Debug, PartialEq, Eq, Clone, TypeInfo)]
pub enum Never {}

/// A pallet identifier. These are per pallet and should be stored in a registry somewhere.
#[derive(Clone, Copy, Eq, PartialEq, Encode, Decode, TypeInfo)]
pub struct PalletId(pub [u8; 8]);

impl TypeId for PalletId {
	const TYPE_ID: [u8; 4] = *b"modl";
}

/// Generate a [`#[pallet::storage]`](pallet_macros::storage) alias outside of a pallet.
///
/// This storage alias works similarly to the [`#[pallet::storage]`](pallet_macros::storage)
/// attribute macro. It supports [`StorageValue`](storage::types::StorageValue),
/// [`StorageMap`](storage::types::StorageMap),
/// [`StorageDoubleMap`](storage::types::StorageDoubleMap) and
/// [`StorageNMap`](storage::types::StorageNMap). The main difference to the normal
/// [`#[pallet::storage]`](pallet_macros::storage) is the flexibility around declaring the
/// storage prefix to use. The storage prefix determines where to find the value in the
/// storage. [`#[pallet::storage]`](pallet_macros::storage) uses the name of the pallet as
/// declared in [`construct_runtime!`].
///
/// The flexibility around declaring the storage prefix makes this macro very useful for
/// writing migrations etc.
///
/// # Examples
///
/// There are different ways to declare the `prefix` to use. The `prefix` type can either be
/// declared explicitly by passing it to the macro as an attribute or by letting the macro
/// guess on what the `prefix` type is. The `prefix` is always passed as the first generic
/// argument to the type declaration. When using [`#[pallet::storage]`](pallet_macros::storage)
/// this first generic argument is always `_`. Besides declaring the `prefix`, the rest of the
/// type declaration works as with [`#[pallet::storage]`](pallet_macros::storage).
///
/// 1. Use the `verbatim` prefix type. This prefix type uses the given identifier as the
/// `prefix`:
#[doc = docify::embed!("src/tests/storage_alias.rs", verbatim_attribute)]
/// 2. Use the `pallet_name` prefix type. This prefix type uses the name of the pallet as
/// configured in    [`construct_runtime!`] as the `prefix`:
#[doc = docify::embed!("src/tests/storage_alias.rs", pallet_name_attribute)]
/// It requires that the given prefix type implements
/// [`PalletInfoAccess`](traits::PalletInfoAccess) (which is always the case for FRAME pallet
/// structs). In the example above, `Pallet<T>` is the prefix type.
///
/// 3. Use the `dynamic` prefix type. This prefix type calls [`Get::get()`](traits::Get::get)
///    to get the `prefix`:
#[doc = docify::embed!("src/tests/storage_alias.rs", dynamic_attribute)]
/// It requires that the given prefix type implements [`Get<'static str>`](traits::Get).
///
/// 4. Let the macro "guess" what kind of prefix type to use. This only supports verbatim or
///    pallet name. The macro uses the presence of generic arguments to the prefix type as an
///    indication that it should use the pallet name as the `prefix`:
#[doc = docify::embed!("src/tests/storage_alias.rs", storage_alias_guess)]
pub use frame_support_procedural::storage_alias;

pub use frame_support_procedural::stored;

pub use frame_support_procedural::derive_impl;

/// Experimental macros for defining dynamic params that can be used in pallet configs.
#[cfg(feature = "experimental")]
pub mod dynamic_params {
	pub use frame_support_procedural::{
		dynamic_aggregated_params_internal, dynamic_pallet_params, dynamic_params,
	};
}

#[doc(inline)]
pub use frame_support_procedural::{
	construct_runtime, match_and_insert, transactional, PalletError,
};

pub use frame_support_procedural::runtime;

#[doc(hidden)]
pub use frame_support_procedural::{__create_tt_macro, __generate_dummy_part_checker};

/// Derive [`Clone`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::CloneNoBound;
/// trait Config {
/// 		type C: Clone;
/// }
///
/// // Foo implements [`Clone`] because `C` bounds [`Clone`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`Clone`].
/// #[derive(CloneNoBound)]
/// struct Foo<T: Config> {
/// 		c: T::C,
/// }
/// ```
pub use frame_support_procedural::CloneNoBound;

/// Derive [`Eq`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::{EqNoBound, PartialEqNoBound};
/// trait Config {
/// 		type C: Eq;
/// }
///
/// // Foo implements [`Eq`] because `C` bounds [`Eq`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`Eq`].
/// #[derive(PartialEqNoBound, EqNoBound)]
/// struct Foo<T: Config> {
/// 		c: T::C,
/// }
/// ```
pub use frame_support_procedural::EqNoBound;

/// Derive [`PartialEq`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::PartialEqNoBound;
/// trait Config {
/// 		type C: PartialEq;
/// }
///
/// // Foo implements [`PartialEq`] because `C` bounds [`PartialEq`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`PartialEq`].
/// #[derive(PartialEqNoBound)]
/// struct Foo<T: Config> {
/// 		c: T::C,
/// }
/// ```
pub use frame_support_procedural::PartialEqNoBound;

/// Derive [`Ord`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::{OrdNoBound, PartialOrdNoBound, EqNoBound, PartialEqNoBound};
/// trait Config {
/// 		type C: Ord;
/// }
///
/// // Foo implements [`Ord`] because `C` bounds [`Ord`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`Ord`].
/// #[derive(EqNoBound, OrdNoBound, PartialEqNoBound, PartialOrdNoBound)]
/// struct Foo<T: Config> {
/// 		c: T::C,
/// }
/// ```
pub use frame_support_procedural::OrdNoBound;

/// Derive [`PartialOrd`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::{OrdNoBound, PartialOrdNoBound, EqNoBound, PartialEqNoBound};
/// trait Config {
/// 		type C: PartialOrd;
/// }
///
/// // Foo implements [`PartialOrd`] because `C` bounds [`PartialOrd`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`PartialOrd`].
/// #[derive(PartialOrdNoBound, PartialEqNoBound, EqNoBound)]
/// struct Foo<T: Config> {
/// 		c: T::C,
/// }
/// ```
pub use frame_support_procedural::PartialOrdNoBound;

/// Derive [`Debug`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::DebugNoBound;
/// # use core::fmt::Debug;
/// trait Config {
/// 		type C: Debug;
/// }
///
/// // Foo implements [`Debug`] because `C` bounds [`Debug`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`Debug`].
/// #[derive(DebugNoBound)]
/// struct Foo<T: Config> {
/// 		c: T::C,
/// }
/// ```
pub use frame_support_procedural::DebugNoBound;

/// Derive [`Default`] but do not bound any generic.
///
/// This is useful for type generic over runtime:
/// ```
/// # use frame_support::DefaultNoBound;
/// # use core::default::Default;
/// trait Config {
/// 	type C: Default;
/// }
///
/// // Foo implements [`Default`] because `C` bounds [`Default`].
/// // Otherwise compilation will fail with an output telling `c` doesn't implement [`Default`].
/// #[derive(DefaultNoBound)]
/// struct Foo<T: Config> {
/// 	c: T::C,
/// }
///
/// // Also works with enums, by specifying the default with #[default]:
/// #[derive(DefaultNoBound)]
/// enum Bar<T: Config> {
/// 	// Bar will implement Default as long as all of the types within Baz also implement default.
/// 	#[default]
/// 	Baz(T::C),
/// 	Quxx,
/// }
/// ```
pub use frame_support_procedural::DefaultNoBound;

/// Assert the annotated function is executed within a storage transaction.
///
/// The assertion is enabled for native execution and when `debug_assertions` are enabled.
///
/// # Example
///
/// ```
/// # use frame_support::{
/// # 	require_transactional, transactional, dispatch::DispatchResult
/// # };
///
/// #[require_transactional]
/// fn update_all(value: u32) -> DispatchResult {
/// 	// Update multiple storages.
/// 	// Return `Err` to indicate should revert.
/// 	Ok(())
/// }
///
/// #[transactional]
/// fn safe_update(value: u32) -> DispatchResult {
/// 	// This is safe
/// 	update_all(value)
/// }
///
/// fn unsafe_update(value: u32) -> DispatchResult {
/// 	// this may panic if unsafe_update is not called within a storage transaction
/// 	update_all(value)
/// }
/// ```
pub use frame_support_procedural::require_transactional;

/// Convert the current crate version into a [`CrateVersion`](crate::traits::CrateVersion).
///
/// It uses the `CARGO_PKG_VERSION_MAJOR`, `CARGO_PKG_VERSION_MINOR` and
/// `CARGO_PKG_VERSION_PATCH` environment variables to fetch the crate version.
/// This means that the [`CrateVersion`](crate::traits::CrateVersion)
/// object will correspond to the version of the crate the macro is called in!
///
/// # Example
///
/// ```
/// # use frame_support::{traits::CrateVersion, crate_to_crate_version};
/// const Version: CrateVersion = crate_to_crate_version!();
/// ```
pub use frame_support_procedural::crate_to_crate_version;

#[doc(hidden)]
pub use serde::{Deserialize, Serialize};

#[doc(hidden)]
pub use macro_magic;

pub use derive_where;

/// Prelude to be used for pallet testing, for ease of use.
#[cfg(feature = "std")]
pub mod testing_prelude {
	pub use super::traits::Get;
	pub use crate::{
		assert_err, assert_err_ignore_postinfo, assert_err_with_weight, assert_noop, assert_ok,
		assert_storage_noop, parameter_types,
	};
	pub use sp_arithmetic::assert_eq_error_rate;
	pub use sp_runtime::{bounded_btree_map, bounded_vec};
}

/// Prelude to be used alongside pallet macro, for ease of use.
pub mod pallet_prelude {
	pub use crate::{
		defensive, defensive_assert,
		dispatch::{DispatchClass, DispatchResult, DispatchResultWithPostInfo, Parameter, Pays},
		ensure,
		inherent::{InherentData, InherentIdentifier, ProvideInherent},
		storage,
		storage::{
			bounded_btree_map::BoundedBTreeMap,
			bounded_btree_set::BoundedBTreeSet,
			bounded_vec::BoundedVec,
			types::{
				CountedStorageMap, CountedStorageNMap, Key as NMapKey, OptionQuery, ResultQuery,
				StorageDoubleMap, StorageMap, StorageNMap, StorageValue, ValueQuery,
			},
			weak_bounded_vec::WeakBoundedVec,
			StorageList,
		},
		traits::{
			Authorize, BuildGenesisConfig, ConstU32, ConstUint, EnsureOrigin, Get, GetDefault,
			GetStorageVersion, Hooks, IsType, OriginTrait, PalletInfoAccess, StorageInfoTrait,
			StorageVersion, Task, TypedGet,
		},
		Blake2_128, Blake2_128Concat, Blake2_256, CloneNoBound, DebugNoBound, EqNoBound, Identity,
		PartialEqNoBound, Twox128, Twox256, Twox64Concat,
	};
	pub use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
	pub use core::marker::PhantomData;
	pub use frame_support::pallet_macros::*;
	pub use frame_support_procedural::{inject_runtime_type, register_default_impl};
	pub use scale_info::TypeInfo;
	pub use sp_inherents::MakeFatalError;
	pub use sp_runtime::{
		traits::{
			CheckedAdd, CheckedConversion, CheckedDiv, CheckedMul, CheckedShl, CheckedShr,
			CheckedSub, MaybeSerializeDeserialize, Member, One, ValidateResult, Zero,
		},
		transaction_validity::{
			InvalidTransaction, TransactionLongevity, TransactionPriority, TransactionSource,
			TransactionTag, TransactionValidity, TransactionValidityError,
			TransactionValidityWithRefund, UnknownTransaction, ValidTransaction,
		},
		Debug, DispatchError, MAX_MODULE_ERROR_ENCODED_SIZE,
	};
	pub use sp_weights::Weight;

	#[allow(deprecated)]
	pub use sp_runtime::traits::ValidateUnsigned;
}

/// The pallet macro has 2 purposes:
///
/// * [For declaring a pallet as a rust module](#1---pallet-module-declaration)
/// * [For declaring the `struct` placeholder of a
///   pallet](#2---pallet-struct-placeholder-declaration)
///
/// # 1 - Pallet module declaration
///
/// The module to declare a pallet is organized as follows:
/// ```
/// #[frame_support::pallet]    // <- the macro
/// mod pallet {
/// 	#[pallet::pallet]
/// 	pub struct Pallet<T>(_);
///
/// 	#[pallet::config]
/// 	pub trait Config: frame_system::Config {}
///
/// 	#[pallet::call]
/// 	impl<T: Config> Pallet<T> {
/// 	}
///
/// 	/* ... */
/// }
/// ```
///
/// The documentation for each individual part can be found at [frame_support::pallet_macros]
///
/// ## Dev Mode (`#[pallet(dev_mode)]`)
///
/// Syntax:
///
/// ```
/// #[frame_support::pallet(dev_mode)]
/// mod pallet {
/// # 	 #[pallet::pallet]
/// # 	 pub struct Pallet<T>(_);
/// # 	 #[pallet::config]
/// # 	 pub trait Config: frame_system::Config {}
/// 	/* ... */
/// }
/// ```
///
/// Specifying the argument `dev_mode` will allow you to enable dev mode for a pallet. The
/// aim of dev mode is to loosen some of the restrictions and requirements placed on
/// production pallets for easy tinkering and development. Dev mode pallets should not be
/// used in production. Enabling dev mode has the following effects:
///
/// * Weights no longer need to be specified on every `#[pallet::call]` declaration. By
///   default, dev mode pallets will assume a weight of zero (`0`) if a weight is not
///   specified. This is equivalent to specifying `#[weight(0)]` on all calls that do not
///   specify a weight.
/// * Call indices no longer need to be specified on every `#[pallet::call]` declaration. By
///   default, dev mode pallets will assume a call index based on the order of the call.
/// * All storages are marked as unbounded, meaning you do not need to implement
///   [`MaxEncodedLen`](frame_support::pallet_prelude::MaxEncodedLen) on storage types. This is
///   equivalent to specifying `#[pallet::unbounded]` on all storage type definitions.
/// * Storage hashers no longer need to be specified and can be replaced by `_`. In dev mode,
///   these will be replaced by `Blake2_128Concat`. In case of explicit key-binding, `Hasher`
///   can simply be ignored when in `dev_mode`.
///
/// Note that the `dev_mode` argument can only be supplied to the `#[pallet]` or
/// `#[frame_support::pallet]` attribute macro that encloses your pallet module. This
/// argument cannot be specified anywhere else, including but not limited to the
/// `#[pallet::pallet]` attribute macro.
///
/// <div class="example-wrap" style="display:inline-block"><pre class="compile_fail"
/// style="white-space:normal;font:inherit;">
/// <strong>WARNING</strong>:
/// You should never deploy or use dev mode pallets in production. Doing so can break your
/// chain. Once you are done tinkering, you should
/// remove the 'dev_mode' argument from your #[pallet] declaration and fix any compile
/// errors before attempting to use your pallet in a production scenario.
/// </pre></div>
///
/// # 2 - Pallet struct placeholder declaration
///
/// The pallet struct placeholder `#[pallet::pallet]` is mandatory and allows you to
/// specify pallet information.
///
/// The struct must be defined as follows:
/// ```
/// #[frame_support::pallet]
/// mod pallet {
/// 	#[pallet::pallet]         // <- the macro
/// 	pub struct Pallet<T>(_);  // <- the struct definition
///
/// 	#[pallet::config]
/// 	pub trait Config: frame_system::Config {}
/// }
/// ```
//
/// I.e. a regular struct definition named `Pallet`, with generic T and no where clause.
///
/// ## Macro expansion:
///
/// The macro adds this attribute to the Pallet struct definition:
/// ```ignore
/// #[derive(
/// 	frame_support::CloneNoBound,
/// 	frame_support::EqNoBound,
/// 	frame_support::PartialEqNoBound,
/// 	frame_support::DebugNoBound,
/// )]
/// ```
/// and replaces the type `_` with `PhantomData<T>`.
///
/// It also implements on the pallet:
///
/// * [`GetStorageVersion`](frame_support::traits::GetStorageVersion)
/// * [`OnGenesis`](frame_support::traits::OnGenesis): contains some logic to write the pallet
///   version into storage.
/// * [`PalletInfoAccess`](frame_support::traits::PalletInfoAccess) to ease access to pallet
///   information given by [`frame_support::traits::PalletInfo`]. (The implementation uses the
///   associated type [`frame_support::traits::PalletInfo`]).
/// * [`StorageInfoTrait`](frame_support::traits::StorageInfoTrait) to give information about
///   storages.
///
/// If the attribute `set_storage_max_encoded_len` is set then the macro calls
/// [`StorageInfoTrait`](frame_support::traits::StorageInfoTrait) for each storage in the
/// implementation of [`StorageInfoTrait`](frame_support::traits::StorageInfoTrait) for the
/// pallet. Otherwise, it implements
/// [`StorageInfoTrait`](frame_support::traits::StorageInfoTrait) for the pallet using the
/// [`PartialStorageInfoTrait`](frame_support::traits::PartialStorageInfoTrait)
/// implementation of storages.
///
/// ## Note on deprecation.
///
/// - Usage of `deprecated` attribute will propagate deprecation information to the pallet
///   metadata.
/// - For general usage examples of `deprecated` attribute please refer to <https://doc.rust-lang.org/nightly/reference/attributes/diagnostics.html#the-deprecated-attribute>
/// - Usage of `allow(deprecated)` on the item will propagate this attribute to the generated
///   code.
/// - If the item is annotated with `deprecated` attribute then the generated code will be
///   automatically annotated with `allow(deprecated)`
pub use frame_support_procedural::pallet;

/// Contains macro stubs for all of the `pallet::` macros
pub mod pallet_macros {
	/// Declare the storage as whitelisted from benchmarking.
	///
	/// Doing so will exclude reads of that value's storage key from counting towards weight
	/// calculations during benchmarking.
	///
	/// This attribute should only be attached to storages that are known to be
	/// read/used in every block. This will result in a more accurate benchmarking weight.
	///
	/// ### Example
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::storage]
	/// 	#[pallet::whitelist_storage]
	/// 	pub type MyStorage<T> = StorageValue<_, u32>;
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	pub use frame_support_procedural::whitelist_storage;

	/// Allows specifying the weight of a call.
	///
	/// Each dispatchable needs to define a weight.
	/// This attribute allows to define a weight using the expression:
	/// `#[pallet::weight($expr)]` Note that argument of the call are available inside the
	/// expression.
	///
	/// If not defined explicitly, the weight can be implicitly inferred from the weight info
	/// defined in the attribute `pallet::call`: `#[pallet::call(weight = $WeightInfo)]`.
	/// Or it can be simply ignored when the pallet is in `dev_mode`.
	///
	/// ## Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///  	use frame_support::pallet_prelude::*;
	///  	use frame_system::pallet_prelude::*;
	///
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	///  	#[pallet::config]
	///  	pub trait Config: frame_system::Config {
	///         /// Type for specifying dispatchable weights.
	///         type WeightInfo: WeightInfo;
	///     }
	///
	/// 	#[pallet::call(weight = <T as Config>::WeightInfo)]
	/// 	impl<T: Config> Pallet<T> {
	/// 		// Explicit weight definition
	/// 		#[pallet::weight(<T as Config>::WeightInfo::do_something())]
	/// 		#[pallet::call_index(0)]
	/// 		pub fn do_something(
	/// 			origin: OriginFor<T>,
	/// 			foo: u32,
	/// 		) -> DispatchResult {
	/// 			Ok(())
	/// 		}
	///
	///             // Implicit weight definition, the macro looks up to the weight info defined in
	///             // `#[pallet::call(weight = $WeightInfo)]` attribute. Then use
	///             // `$WeightInfo::do_something_else` as the weight function.
	///             #[pallet::call_index(1)]
	///             pub fn do_something_else(
	///                 origin: OriginFor<T>,
	///                 bar: u64,
	///             ) -> DispatchResult {
	///                 Ok(())
	///             }
	///     }
	///
	///     /// The `WeightInfo` trait defines weight functions for dispatchable calls.
	///     pub trait WeightInfo {
	///         fn do_something() -> Weight;
	///         fn do_something_else() -> Weight;
	///     }
	/// }
	/// ```
	pub use frame_support_procedural::weight;

	/// Allows whitelisting a storage item from decoding during try-runtime checks.
	///
	/// The optional attribute `#[pallet::disable_try_decode_storage]` will declare the
	/// storage as whitelisted from decoding during try-runtime checks. This should only be
	/// attached to transient storage which cannot be migrated during runtime upgrades.
	///
	/// ### Example
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::storage]
	/// 	#[pallet::disable_try_decode_storage]
	/// 	pub type MyStorage<T> = StorageValue<_, u32>;
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	pub use frame_support_procedural::disable_try_decode_storage;

	/// Declares a storage as unbounded in potential size.
	///
	/// When implementing the storage info (when `#[pallet::generate_storage_info]` is
	/// specified on the pallet struct placeholder), the size of the storage will be declared
	/// as unbounded. This can be useful for storage which can never go into PoV (Proof of
	/// Validity).
	///
	/// ## Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::storage]
	/// 	#[pallet::unbounded]
	/// 	pub type MyStorage<T> = StorageValue<_, u32>;
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	pub use frame_support_procedural::unbounded;

	/// Defines what storage prefix to use for a storage item when building the trie.
	///
	/// This is helpful if you wish to rename the storage field but don't want to perform a
	/// migration.
	///
	/// ## Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::storage]
	/// 	#[pallet::storage_prefix = "foo"]
	/// 	pub type MyStorage<T> = StorageValue<_, u32>;
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	pub use frame_support_procedural::storage_prefix;

	/// Ensures the generated `DefaultConfig` will not have any bounds for
	/// that trait item.
	///
	/// Attaching this attribute to a trait item ensures that the generated trait
	/// `DefaultConfig` will not have any bounds for this trait item.
	///
	/// As an example, if you have a trait item `type AccountId: SomeTrait;` in your `Config`
	/// trait, the generated `DefaultConfig` will only have `type AccountId;` with no trait
	/// bound.
	pub use frame_support_procedural::no_default_bounds;

	/// Ensures the trait item will not be used as a default with the
	/// `#[derive_impl(..)]` attribute macro.
	///
	/// The optional attribute `#[pallet::no_default]` can be attached to trait items within a
	/// `Config` trait impl that has [`#[pallet::config(with_default)]`](`config`)
	/// attached.
	pub use frame_support_procedural::no_default;

	/// Declares a module as importable into a pallet via
	/// [`#[import_section]`](`import_section`).
	///
	/// Note that sections are imported by their module name/ident, and should be referred to
	/// by their _full path_ from the perspective of the target pallet. Do not attempt to make
	/// use of `use` statements to bring pallet sections into scope, as this will not work
	/// (unless you do so as part of a wildcard import, in which case it will work).
	///
	/// ## Naming Logistics
	///
	/// Also note that because of how `#[pallet_section]` works, pallet section names must be
	/// globally unique _within the crate in which they are defined_. For more information on
	/// why this must be the case, see macro_magic's
	/// [`#[export_tokens]`](https://docs.rs/macro_magic/latest/macro_magic/attr.export_tokens.html) macro.
	///
	/// Optionally, you may provide an argument to `#[pallet_section]` such as
	/// `#[pallet_section(some_ident)]`, in the event that there is another pallet section in
	/// same crate with the same ident/name. The ident you specify can then be used instead of
	/// the module's ident name when you go to import it via
	/// [`#[import_section]`](`import_section`).
	pub use frame_support_procedural::pallet_section;

	/// The `#[pallet::inherent]` attribute allows the pallet to provide
	/// [inherents](https://docs.substrate.io/fundamentals/transaction-types/#inherent-transactions).
	///
	/// An inherent is some piece of data that is inserted by a block authoring node at block
	/// creation time and can either be accepted or rejected by validators based on whether the
	/// data falls within an acceptable range.
	///
	/// The most common inherent is the `timestamp` that is inserted into every block. Since
	/// there is no way to validate timestamps, validators simply check that the timestamp
	/// reported by the block authoring node falls within an acceptable range.
	///
	/// Example usage:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// # 	use frame_support::inherent::IsFatalError;
	/// # 	use sp_timestamp::InherentError;
	/// # 	use core::result;
	/// #
	/// 	// Example inherent identifier
	/// 	pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"timstap0";
	///
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::inherent]
	/// 	impl<T: Config> ProvideInherent for Pallet<T> {
	/// 		type Call = Call<T>;
	/// 		type Error = InherentError;
	/// 		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;
	///
	/// 		fn create_inherent(data: &InherentData) -> Option<Self::Call> {
	/// 			unimplemented!()
	/// 		}
	///
	/// 		fn check_inherent(
	/// 			call: &Self::Call,
	/// 			data: &InherentData,
	/// 		) -> result::Result<(), Self::Error> {
	/// 			unimplemented!()
	/// 		}
	///
	/// 		fn is_inherent(call: &Self::Call) -> bool {
	/// 			unimplemented!()
	/// 		}
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// I.e. a trait implementation with bound `T: Config`, of trait `ProvideInherent` for type
	/// `Pallet<T>`, and some optional where clause.
	///
	/// ## Macro expansion
	///
	/// The macro currently makes no use of this information, but it might use this information
	/// in the future to give information directly to `construct_runtime`.
	pub use frame_support_procedural::inherent;

	/// Splits a pallet declaration into multiple parts.
	///
	/// An attribute macro that can be attached to a module declaration. Doing so will
	/// import the contents of the specified external pallet section that is defined
	/// elsewhere using [`#[pallet_section]`](`pallet_section`).
	///
	/// ## Example
	/// ```
	/// # use frame_support::pallet_macros::pallet_section;
	/// # use frame_support::pallet_macros::import_section;
	/// #
	/// /// A [`pallet_section`] that defines the events for a pallet.
	/// /// This can later be imported into the pallet using [`import_section`].
	/// #[pallet_section]
	/// mod events {
	/// 	#[pallet::event]
	/// 	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	/// 	pub enum Event<T: Config> {
	/// 		/// Event documentation should end with an array that provides descriptive names for event
	/// 		/// parameters. [something, who]
	/// 		SomethingStored { something: u32, who: T::AccountId },
	/// 	}
	/// }
	///
	/// #[import_section(events)]
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config<RuntimeEvent: From<Event<Self>>> {
	/// # 	}
	/// }
	/// ```
	///
	/// This will result in the contents of `some_section` being _verbatim_ imported into
	/// the pallet above. Note that since the tokens for `some_section` are essentially
	/// copy-pasted into the target pallet, you cannot refer to imports that don't also
	/// exist in the target pallet, but this is easily resolved by including all relevant
	/// `use` statements within your pallet section, so they are imported as well, or by
	/// otherwise ensuring that you have the same imports on the target pallet.
	///
	/// It is perfectly permissible to import multiple pallet sections into the same pallet,
	/// which can be done by having multiple `#[import_section(something)]` attributes
	/// attached to the pallet.
	///
	/// Note that sections are imported by their module name/ident, and should be referred to
	/// by their _full path_ from the perspective of the target pallet.
	pub use frame_support_procedural::import_section;

	/// Allows defining getter functions on `Pallet` storage.
	///
	/// ## Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::storage]
	/// 	#[pallet::getter(fn my_getter_fn_name)]
	/// 	pub type MyStorage<T> = StorageValue<_, u32>;
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// See [`pallet::storage`](`frame_support::pallet_macros::storage`) for more info.
	pub use frame_support_procedural::getter;

	/// Defines constants that are added to the constant field of
	/// [`PalletMetadata`](frame_metadata::v15::PalletMetadata) struct for this pallet.
	///
	/// Must be defined like:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// #
	/// 	#[pallet::extra_constants]
	/// 	impl<T: Config> Pallet<T> // $optional_where_clause
	/// 	{
	/// 	#[pallet::constant_name(SomeU32ConstantName)]
	/// 		/// Some doc
	/// 		fn some_u32_constant() -> u32 {
	/// 			100u32
	/// 		}
	/// 	}
	/// }
	/// ```
	///
	/// I.e. a regular rust `impl` block with some optional where clause and functions with 0
	/// args, 0 generics, and some return type.
	pub use frame_support_procedural::extra_constants;

	#[rustfmt::skip]
	/// Allows bypassing the `frame_system::Config` supertrait check.
	///
	/// To bypass the syntactic `frame_system::Config` supertrait check, use the attribute
	/// `pallet::disable_frame_system_supertrait_check`.
	///
	/// Note this bypass is purely syntactic, and does not actually remove the requirement that your
	/// pallet implements `frame_system::Config`. When using this check, your config is still required to implement
	/// `frame_system::Config` either via
	/// - Implementing a trait that itself implements `frame_system::Config`
	/// - Tightly coupling it with another pallet which itself implements `frame_system::Config`
	///
	/// e.g.
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// # 	use frame_system::pallet_prelude::*;
	/// 	trait OtherTrait: frame_system::Config {}
	///
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::config]
	/// 	#[pallet::disable_frame_system_supertrait_check]
	/// 	pub trait Config: OtherTrait {}
	/// }
	/// ```
	///
	/// To learn more about supertraits, see the
	/// [trait_based_programming](../../polkadot_sdk_docs/reference_docs/trait_based_programming/index.html)
	/// reference doc.
	pub use frame_support_procedural::disable_frame_system_supertrait_check;

	/// The mandatory attribute allowing definition of configurable types for the pallet.
	///
	/// Item must be defined as:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::config]
	/// 	pub trait Config: frame_system::Config // + $optionally_some_other_supertraits
	/// 	// $optional_where_clause
	/// 	{
	/// 		// config items here
	/// 	}
	/// }
	/// ```
	///
	/// I.e. a regular trait definition named `Config`, with the supertrait
	/// [`frame_system::pallet::Config`](../../frame_system/pallet/trait.Config.html), and
	/// optionally other supertraits and a where clause. (Specifying other supertraits here is
	/// known as [tight coupling](https://docs.substrate.io/reference/how-to-guides/pallet-design/use-tight-coupling/))
	///
	/// ## Optional: `with_default`
	///
	/// An optional `with_default` argument may also be specified. Doing so will automatically
	/// generate a `DefaultConfig` trait inside your pallet which is suitable for use with
	/// [`#[derive_impl(..)`](`frame_support::derive_impl`) to derive a default testing
	/// config:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// # 	use frame_system::pallet_prelude::*;
	/// # 	use core::fmt::Debug;
	/// # 	use frame_support::traits::Contains;
	/// #
	/// # 	pub trait SomeMoreComplexBound {}
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::config(with_default)] // <- with_default is optional
	/// 	pub trait Config: frame_system::Config {
	/// 		/// A more complex type.
	/// 		#[pallet::no_default] // Example of type where no default should be provided
	/// 		type MoreComplexType: SomeMoreComplexBound;
	///
	/// 		/// A simple type.
	/// 		// Default with bounds is supported for simple types
	/// 		type SimpleType: From<u32>;
	/// 	}
	///
	/// 	#[pallet::event]
	/// 	pub enum Event<T: Config> {
	/// 		SomeEvent(u16, u32),
	/// 	}
	/// }
	/// ```
	///
	/// As shown above:
	/// * you may attach the [`#[pallet::no_default]`](`no_default`)
	/// attribute to specify that a particular trait item _cannot_ be used as a default when a
	/// test `Config` is derived using the [`#[derive_impl(..)]`](`frame_support::derive_impl`)
	/// attribute macro. This will cause that particular trait item to simply not appear in
	/// default testing configs based on this config (the trait item will not be included in
	/// `DefaultConfig`).
	/// * you may attach the [`#[pallet::no_default_bounds]`](`no_default_bounds`)
	/// attribute to specify that a particular trait item can be used as a default when a
	/// test `Config` is derived using the [`#[derive_impl(..)]`](`frame_support::derive_impl`)
	/// attribute macro. But its bounds cannot be enforced at this point and should be
	/// discarded when generating the default config trait.
	/// * you may not specify any attribute to generate a trait item in the default config
	///   trait.
	///
	/// In case origin of error is not clear it is recommended to disable all default with
	/// [`#[pallet::no_default]`](`no_default`) and enable them one by one.
	///
	/// ### `DefaultConfig` Caveats
	///
	/// The auto-generated `DefaultConfig` trait:
	/// - is always a _subset_ of your pallet's `Config` trait.
	/// - can only contain items that don't rely on externalities, such as
	///   `frame_system::Config`.
	///
	/// Trait items that _do_ rely on externalities should be marked with
	/// [`#[pallet::no_default]`](`no_default`)
	///
	/// Consequently:
	/// - Any items that rely on externalities _must_ be marked with
	///   [`#[pallet::no_default]`](`no_default`) or your trait will fail to compile when used
	///   with [`derive_impl`](`frame_support::derive_impl`).
	/// - Items marked with [`#[pallet::no_default]`](`no_default`) are entirely excluded from
	///   the `DefaultConfig` trait, and therefore any impl of `DefaultConfig` doesn't need to
	///   implement such items.
	///
	/// For more information, see:
	/// * [`frame_support::derive_impl`].
	/// * [`#[pallet::no_default]`](`no_default`)
	/// * [`#[pallet::no_default_bounds]`](`no_default_bounds`)
	///
	/// ## Optional: `without_automatic_metadata`
	///
	/// By default, the associated types of the `Config` trait that require the `TypeInfo` or
	/// `Parameter` bounds are included in the metadata of the pallet.
	///
	/// The optional `without_automatic_metadata` argument can be used to exclude these
	/// associated types from the metadata collection.
	///
	/// Furthermore, the `without_automatic_metadata` argument can be used in combination with
	/// the [`#[pallet::include_metadata]`](`include_metadata`) attribute to selectively
	/// include only certain associated types in the metadata collection.
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// # 	use frame_system::pallet_prelude::*;
	/// # 	use core::fmt::Debug;
	/// # 	use frame_support::traits::{Contains, VariantCount};
	/// #
	/// # 	pub trait SomeMoreComplexBound {}
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::config(with_default, without_automatic_metadata)] // <- with_default and without_automatic_metadata are optional
	/// 	pub trait Config: frame_system::Config {
	/// 		/// The overarching freeze reason.
	/// 		#[pallet::no_default_bounds] // Default with bounds is not supported for RuntimeFreezeReason
	/// 		type RuntimeFreezeReason: Parameter + Member + MaxEncodedLen + Copy + VariantCount;
	/// 		/// A simple type.
	/// 		// Type that would have been included in metadata, but is now excluded.
	/// 		type SimpleType: From<u32> + TypeInfo;
	///
	/// 		// The `pallet::include_metadata` is used to selectively include this type in metadata.
	/// 		#[pallet::include_metadata]
	/// 		type SelectivelyInclude: From<u32> + TypeInfo;
	/// 	}
	///
	/// 	#[pallet::event]
	/// 	pub enum Event<T: Config> {
	/// 		SomeEvent(u16, u32),
	/// 	}
	/// }
	/// ```
	pub use frame_support_procedural::config;

	/// Allows defining an enum that gets composed as an aggregate enum by `construct_runtime`.
	///
	/// The `#[pallet::composite_enum]` attribute allows you to define an enum that gets
	/// composed as an aggregate enum by `construct_runtime`. This is similar in principle with
	/// [frame_support_procedural::event] and [frame_support_procedural::error].
	///
	/// The attribute currently only supports enum definitions, and identifiers that are named
	/// `FreezeReason`, `HoldReason`, `LockId` or `SlashReason`. Arbitrary identifiers for the
	/// enum are not supported. The aggregate enum generated by
	/// [`frame_support::construct_runtime`] will have the name of `RuntimeFreezeReason`,
	/// `RuntimeHoldReason`, `RuntimeLockId` and `RuntimeSlashReason` respectively.
	///
	/// NOTE: The aggregate enum generated by `construct_runtime` generates a conversion
	/// function from the pallet enum to the aggregate enum, and automatically derives the
	/// following traits:
	///
	/// ```ignore
	/// Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Encode, Decode, MaxEncodedLen, TypeInfo,
	/// Debug
	/// ```
	///
	/// For ease of usage, when no `#[derive]` attributes are found for the enum under
	/// [`#[pallet::composite_enum]`](composite_enum), the aforementioned traits are
	/// automatically derived for it. The inverse is also true: if there are any `#[derive]`
	/// attributes found for the enum, then no traits will automatically be derived for it.
	///
	/// e.g, defining `HoldReason` in a pallet
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::composite_enum]
	/// 	pub enum HoldReason {
	/// 		/// The NIS Pallet has reserved it for a non-fungible receipt.
	/// 		#[codec(index = 0)]
	/// 		SomeHoldReason,
	/// 		#[codec(index = 1)]
	/// 		SomeOtherHoldReason,
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	pub use frame_support_procedural::composite_enum;

	/// Deprecation Notice
	///
	/// The `#[pallet::validate_unsigned]` attribute has been deprecated and will be removed in
	/// a future release. Use [`sp_runtime::traits::TransactionExtension`] instead.
	///
	/// For more information, see: <https://github.com/paritytech/polkadot-sdk/issues/2415>
	/// ---
	/// Allows the pallet to validate unsigned transactions.
	///
	/// Item must be defined as:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::validate_unsigned]
	/// 	impl<T: Config> sp_runtime::traits::ValidateUnsigned for Pallet<T> {
	/// 		type Call = Call<T>;
	///
	/// 		fn validate_unsigned(_source: TransactionSource, _call: &Self::Call) -> TransactionValidity {
	/// 			// Your implementation details here
	/// 			unimplemented!()
	/// 		}
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// I.e. a trait implementation with bound `T: Config`, of trait
	/// [`ValidateUnsigned`](frame_support::pallet_prelude::ValidateUnsigned) for
	/// type `Pallet<T>`, and some optional where clause.
	///
	/// NOTE: There is also the [`sp_runtime::traits::TransactionExtension`] trait that can be
	/// used to add some specific logic for transaction validation.
	///
	/// ## Macro expansion
	///
	/// The macro currently makes no use of this information, but it might use this information
	/// in the future to give information directly to [`frame_support::construct_runtime`].
	pub use frame_support_procedural::validate_unsigned;

	/// Allows defining	view functions on a pallet.
	///
	/// A pallet view function is a read-only function providing access to the state of the
	/// pallet from both outside and inside the runtime. It should provide a _stable_ interface
	/// for querying the state of the pallet, avoiding direct storage access and upgrading
	/// along with the runtime.
	///
	/// ## Syntax
	/// View functions methods must be read-only and always return some output. A
	/// `view_functions` impl block only allows methods to be defined inside of
	/// it.
	///
	/// ## Example
	/// ```
	/// #[frame_support::pallet]
	/// pub mod pallet {
	/// 	use frame_support::pallet_prelude::*;
	///
	///  	#[pallet::config]
	///  	pub trait Config: frame_system::Config {}
	///
	///  	#[pallet::pallet]
	///  	pub struct Pallet<T>(_);
	///
	///     #[pallet::storage]
	/// 	pub type SomeMap<T: Config> = StorageMap<_, Twox64Concat, u32, u32, OptionQuery>;
	///
	///     #[pallet::view_functions]
	///     impl<T: Config> Pallet<T> {
	/// 		/// Retrieve a map storage value by key.
	///         pub fn get_value_with_arg(key: u32) -> Option<u32> {
	/// 			SomeMap::<T>::get(key)
	/// 		}
	///     }
	/// }
	/// ```
	///
	///
	/// ## Usage and implementation details
	/// To allow outside access to pallet view functions, you need to add a runtime API that
	/// accepts view function queries and dispatches them to the right pallet. You can do that
	/// by implementing the
	/// [`RuntimeViewFunction`](frame_support::view_functions::runtime_api::RuntimeViewFunction)
	/// trait for the runtime inside an [`impl_runtime_apis!`](sp_api::impl_runtime_apis)
	/// block.
	///
	/// The `RuntimeViewFunction` trait implements a hashing-based dispatching mechanism to
	/// dispatch view functions to the right method in the right pallet based on their IDs. A
	/// view function ID depends both on its pallet and on its method signature, so it remains
	/// stable as long as those two elements are not modified. In general, pallet view
	/// functions should expose a _stable_ interface and changes to the method signature are
	/// strongly discouraged. For more details on the dispatching mechanism, see the
	/// [`DispatchViewFunction`](frame_support::view_functions::DispatchViewFunction) trait.
	pub use frame_support_procedural::view_functions;

	/// Allows defining a struct implementing the [`Get`](frame_support::traits::Get) trait to
	/// ease the use of storage types.
	///
	/// This attribute is meant to be used alongside [`#[pallet::storage]`](`storage`) to
	/// define a storage's default value. This attribute can be used multiple times.
	///
	/// Item must be defined as:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use sp_runtime::FixedU128;
	/// # 	use frame_support::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::storage]
	/// 	pub(super) type SomeStorage<T: Config> =
	/// 		StorageValue<_, FixedU128, ValueQuery, DefaultForSomeValue>;
	///
	/// 	// Define default for ParachainId
	/// 	#[pallet::type_value]
	/// 	pub fn DefaultForSomeValue() -> FixedU128 {
	/// 		FixedU128::from_u32(1)
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// ## Macro expansion
	///
	/// The macro renames the function to some internal name, generates a struct with the
	/// original name of the function and its generic, and implements `Get<$ReturnType>` by
	/// calling the user defined function.
	pub use frame_support_procedural::type_value;

	/// Allows defining a storage version for the pallet.
	///
	/// Because the `pallet::pallet` macro implements
	/// [`GetStorageVersion`](frame_support::traits::GetStorageVersion), the current storage
	/// version needs to be communicated to the macro. This can be done by using the
	/// `pallet::storage_version` attribute:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::StorageVersion;
	/// # 	use frame_support::traits::GetStorageVersion;
	/// #
	/// 	const STORAGE_VERSION: StorageVersion = StorageVersion::new(5);
	///
	/// 	#[pallet::pallet]
	/// 	#[pallet::storage_version(STORAGE_VERSION)]
	/// 	pub struct Pallet<T>(_);
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// If not present, the current storage version is set to the default value.
	pub use frame_support_procedural::storage_version;

	/// The `#[pallet::hooks]` attribute allows you to specify a
	/// [`frame_support::traits::Hooks`] implementation for `Pallet` that specifies
	/// pallet-specific logic.
	///
	/// The item the attribute attaches to must be defined as follows:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// # 	use frame_system::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::hooks]
	/// 	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
	/// 		// Implement hooks here
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	/// I.e. a regular trait implementation with generic bound: `T: Config`, for the trait
	/// `Hooks<BlockNumberFor<T>>` (they are defined in preludes), for the type `Pallet<T>`.
	///
	/// Optionally, you could add a where clause.
	///
	/// ## Macro expansion
	///
	/// The macro implements the traits
	/// [`OnInitialize`](frame_support::traits::OnInitialize),
	/// [`OnIdle`](frame_support::traits::OnIdle),
	/// [`OnFinalize`](frame_support::traits::OnFinalize),
	/// [`OnRuntimeUpgrade`](frame_support::traits::OnRuntimeUpgrade),
	/// [`OffchainWorker`](frame_support::traits::OffchainWorker), and
	/// [`IntegrityTest`](frame_support::traits::IntegrityTest) using
	/// the provided [`Hooks`](frame_support::traits::Hooks) implementation.
	///
	/// NOTE: `OnRuntimeUpgrade` is implemented with `Hooks::on_runtime_upgrade` and some
	/// additional logic. E.g. logic to write the pallet version into storage.
	///
	/// NOTE: The macro also adds some tracing logic when implementing the above traits. The
	/// following hooks emit traces: `on_initialize`, `on_finalize` and `on_runtime_upgrade`.
	pub use frame_support_procedural::hooks;

	/// Generates a helper function on `Pallet` that handles deposit events.
	///
	/// NOTE: For instantiable pallets, the event must be generic over `T` and `I`.
	///
	/// ## Macro expansion
	///
	/// The macro will add on enum `Event` the attributes:
	/// * `#[derive(`[`frame_support::CloneNoBound`]`)]`
	/// * `#[derive(`[`frame_support::EqNoBound`]`)]`
	/// * `#[derive(`[`frame_support::PartialEqNoBound`]`)]`
	/// * `#[derive(`[`frame_support::DebugNoBound`]`)]`
	/// * `#[derive(`[`codec::Encode`]`)]`
	/// * `#[derive(`[`codec::Decode`]`)]`
	///
	/// The macro implements `From<Event<..>>` for ().
	///
	/// The macro implements a metadata function on `Event` returning the `EventMetadata`.
	///
	/// If `#[pallet::generate_deposit]` is present then the macro implements `fn
	/// deposit_event` on `Pallet`.
	pub use frame_support_procedural::generate_deposit;

	/// Allows defining logic to make an extrinsic call feeless.
	///
	/// Each dispatchable may be annotated with the `#[pallet::feeless_if($closure)]`
	/// attribute, which explicitly defines the condition for the dispatchable to be feeless.
	///
	/// The arguments for the closure must be the referenced arguments of the dispatchable
	/// function.
	///
	/// The closure must return `bool`.
	///
	/// ### Example
	///
	/// ```
	/// #[frame_support::pallet(dev_mode)]
	/// mod pallet {
	/// # 	use frame_support::pallet_prelude::*;
	/// # 	use frame_system::pallet_prelude::*;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::call]
	/// 	impl<T: Config> Pallet<T> {
	/// 		#[pallet::call_index(0)]
	/// 		/// Marks this call as feeless if `foo` is zero.
	/// 		#[pallet::feeless_if(|_origin: &OriginFor<T>, foo: &u32| -> bool {
	/// 			*foo == 0
	/// 		})]
	/// 		pub fn something(
	/// 			_: OriginFor<T>,
	/// 			foo: u32,
	/// 		) -> DispatchResult {
	/// 			unimplemented!()
	/// 		}
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// Please note that this only works for signed dispatchables and requires a transaction
	/// extension such as [`pallet_skip_feeless_payment::SkipCheckIfFeeless`] to wrap the
	/// existing payment extension. Else, this is completely ignored and the dispatchable is
	/// still charged.
	///
	/// Also this will not allow accountless caller to send a transaction if some transaction
	/// extension such as `frame_system::CheckNonce` is used.
	/// Extensions such as `frame_system::CheckNonce` require a funded account to validate
	/// the transaction.
	///
	/// ### Macro expansion
	///
	/// The macro implements the [`pallet_skip_feeless_payment::CheckIfFeeless`] trait on the
	/// dispatchable and calls the corresponding closure in the implementation.
	///
	/// [`pallet_skip_feeless_payment::SkipCheckIfFeeless`]: ../../pallet_skip_feeless_payment/struct.SkipCheckIfFeeless.html
	/// [`pallet_skip_feeless_payment::CheckIfFeeless`]: ../../pallet_skip_feeless_payment/struct.SkipCheckIfFeeless.html
	pub use frame_support_procedural::feeless_if;

	/// Allows defining an error enum that will be returned from the dispatchable when an error
	/// occurs.
	///
	/// The information for this error type is then stored in runtime metadata.
	///
	/// Item must be defined as so:
	///
	/// ```
	/// #[frame_support::pallet(dev_mode)]
	/// mod pallet {
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::error]
	/// 	pub enum Error<T> {
	/// 		/// SomeFieldLessVariant doc
	/// 		SomeFieldLessVariant,
	/// 		/// SomeVariantWithOneField doc
	/// 		SomeVariantWithOneField(u32),
	/// 	}
	/// #
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	/// I.e. a regular enum named `Error`, with generic `T` and fieldless or multiple-field
	/// variants.
	///
	/// Any field type in the enum variants must implement [`scale_info::TypeInfo`] in order to
	/// be properly used in the metadata, and its encoded size should be as small as possible,
	/// preferably 1 byte in size in order to reduce storage size. The error enum itself has an
	/// absolute maximum encoded size specified by
	/// [`frame_support::MAX_MODULE_ERROR_ENCODED_SIZE`].
	///
	/// (1 byte can still be 256 different errors. The more specific the error, the easier it
	/// is to diagnose problems and give a better experience to the user. Don't skimp on having
	/// lots of individual error conditions.)
	///
	/// Field types in enum variants must also implement [`frame_support::PalletError`],
	/// otherwise the pallet will fail to compile. Rust primitive types have already
	/// implemented the [`frame_support::PalletError`] trait along with some commonly used
	/// stdlib types such as [`Option`] and [`core::marker::PhantomData`], and hence
	/// in most use cases, a manual implementation is not necessary and is discouraged.
	///
	/// The generic `T` must not bound anything and a `where` clause is not allowed. That said,
	/// bounds and/or a where clause should not needed for any use-case.
	///
	/// ## Macro expansion
	///
	/// The macro implements the [`Debug`] trait and functions `as_u8` using variant position,
	/// and `as_str` using variant doc.
	///
	/// The macro also implements `From<Error<T>>` for `&'static str` and `From<Error<T>>` for
	/// `DispatchError`.
	///
	/// ## Note on deprecation of Errors
	///
	/// - Usage of `deprecated` attribute will propagate deprecation information to the pallet
	///   metadata where the item was declared.
	/// - For general usage examples of `deprecated` attribute please refer to <https://doc.rust-lang.org/nightly/reference/attributes/diagnostics.html#the-deprecated-attribute>
	/// - It's possible to deprecated either certain variants inside the `Error` or the whole
	///   `Error` itself. If both the `Error` and its variants are deprecated a compile error
	///   will be returned.
	/// - Usage of `allow(deprecated)` on the item will propagate this attribute to the
	///   generated code.
	/// - If the item is annotated with `deprecated` attribute then the generated code will be
	///   automatically annotated with `allow(deprecated)`
	pub use frame_support_procedural::error;

	/// Allows defining pallet events.
	///
	/// Pallet events are stored under the `system` / `events` key when the block is applied
	/// (and then replaced when the next block writes it's events).
	///
	/// The Event enum can be defined as follows:
	///
	/// ```
	/// #[frame_support::pallet(dev_mode)]
	/// mod pallet {
	/// #     use frame_support::pallet_prelude::IsType;
	/// #
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::config]
	/// 	pub trait Config: frame_system::Config {}
	///
	/// 	#[pallet::event]
	/// 	#[pallet::generate_deposit(fn deposit_event)] // Optional
	/// 	pub enum Event<T> {
	/// 		/// SomeEvent doc
	/// 		SomeEvent(u16, u32), // SomeEvent with two fields
	/// 	}
	/// }
	/// ```
	///
	/// I.e. an enum (with named or unnamed fields variant), named `Event`, with generic: none
	/// or `T` or `T: Config`, and optional w here clause.
	///
	/// Macro expansion automatically appends `From<Event<Self>>` bound to
	/// system supertrait's `RuntimeEvent `associated type, i.e:
	///
	/// ```rs
	/// 	#[pallet::config]
	/// 	pub trait Config: frame_system::Config<RuntimeEvent: From<Event<Self>>> {}
	/// ```
	///
	/// Each field must implement [`Clone`], [`Eq`], [`PartialEq`], [`codec::Encode`],
	/// [`codec::Decode`], and [`Debug`] (on std only). For ease of use, bound by the trait
	/// `Member`, available in [`frame_support::pallet_prelude`].
	///
	/// ## Note on deprecation of Events
	///
	/// - Usage of `deprecated` attribute will propagate deprecation information to the pallet
	///   metadata where the item was declared.
	/// - For general usage examples of `deprecated` attribute please refer to <https://doc.rust-lang.org/nightly/reference/attributes/diagnostics.html#the-deprecated-attribute>
	/// - It's possible to deprecated either certain variants inside the `Event` or the whole
	///   `Event` itself. If both the `Event` and its variants are deprecated a compile error
	///   will be returned.
	/// - Usage of `allow(deprecated)` on the item will propagate this attribute to the
	///   generated code.
	/// - If the item is annotated with `deprecated` attribute then the generated code will be
	///   automatically annotated with `allow(deprecated)`
	pub use frame_support_procedural::event;

	/// Selectively includes associated types in the metadata.
	///
	/// The optional attribute allows you to selectively include associated types in the
	/// metadata. This can be attached to trait items that implement `TypeInfo`.
	///
	/// By default all collectable associated types are included in the metadata.
	///
	/// This attribute can be used in combination with the
	/// [`#[pallet::config(without_automatic_metadata)]`](`config`).
	pub use frame_support_procedural::include_metadata;

	/// Allows a pallet to declare a set of functions as a *dispatchable extrinsic*.
	///
	/// In slightly simplified terms, this macro declares the set of "transactions" of a
	/// pallet.
	///
	/// > The exact definition of **extrinsic** can be found in
	/// > [`sp_runtime::generic::UncheckedExtrinsic`].
	///
	/// A **dispatchable** is a common term in FRAME, referring to process of constructing a
	/// function, and dispatching it with the correct inputs. This is commonly used with
	/// extrinsics, for example "an extrinsic has been dispatched". See
	/// [`sp_runtime::traits::Dispatchable`] and [`crate::traits::UnfilteredDispatchable`].
	///
	/// ## Call Enum
	///
	/// The macro is called `call` (rather than `#[pallet::extrinsics]`) because of the
	/// generation of a `enum Call`. This enum contains only the encoding of the function
	/// arguments of the dispatchable, alongside the information needed to route it to the
	/// correct function.
	///
	/// The macro also ensures that the extrinsic when invoked will be wrapped via
	/// [`frame_support::storage::with_storage_layer`] to make it transactional. Thus if the
	/// extrinsic returns with an error any state changes that had already occurred will be
	/// rolled back.
	///
	/// ```
	/// #[frame_support::pallet(dev_mode)]
	/// pub mod custom_pallet {
	/// #   use frame_support::pallet_prelude::*;
	/// #   use frame_system::pallet_prelude::*;
	/// #   #[pallet::config]
	/// #   pub trait Config: frame_system::Config {}
	/// #   #[pallet::pallet]
	/// #   pub struct Pallet<T>(_);
	/// #   use frame_support::traits::BuildGenesisConfig;
	///     #[pallet::call]
	///     impl<T: Config> Pallet<T> {
	///         pub fn some_dispatchable(_origin: OriginFor<T>, _input: u32) -> DispatchResult {
	///             Ok(())
	///         }
	///         pub fn other(_origin: OriginFor<T>, _input: u64) -> DispatchResult {
	///             Ok(())
	///         }
	///     }
	///
	///     // generates something like:
	///     // enum Call<T: Config> {
	///     //  some_dispatchable { input: u32 }
	///     //  other { input: u64 }
	///     // }
	/// }
	///
	/// fn main() {
	/// #   use frame_support::{derive_impl, construct_runtime};
	/// #   use frame_support::__private::codec::Encode;
	/// #   use frame_support::__private::TestExternalities;
	/// #   use frame_support::traits::UnfilteredDispatchable;
	/// #    impl custom_pallet::Config for Runtime {}
	/// #    #[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
	/// #    impl frame_system::Config for Runtime {
	/// #        type Block = frame_system::mocking::MockBlock<Self>;
	/// #    }
	///     construct_runtime! {
	///         pub enum Runtime {
	///             System: frame_system,
	///             Custom: custom_pallet
	///         }
	///     }
	///
	/// #    TestExternalities::new_empty().execute_with(|| {
	///     let origin: RuntimeOrigin = frame_system::RawOrigin::Signed(10).into();
	///     // calling into a dispatchable from within the runtime is simply a function call.
	///         let _ = custom_pallet::Pallet::<Runtime>::some_dispatchable(origin.clone(), 10);
	///
	///     // calling into a dispatchable from the outer world involves constructing the bytes of
	///     let call = custom_pallet::Call::<Runtime>::some_dispatchable { input: 10 };
	///     let _ = call.clone().dispatch_bypass_filter(origin);
	///
	///     // the routing of a dispatchable is simply done through encoding of the `Call` enum,
	///     // which is the index of the variant, followed by the arguments.
	///     assert_eq!(call.encode(), vec![0u8, 10, 0, 0, 0]);
	///
	///     // notice how in the encoding of the second function, the first byte is different and
	///     // referring to the second variant of `enum Call`.
	///     let call = custom_pallet::Call::<Runtime>::other { input: 10 };
	///     assert_eq!(call.encode(), vec![1u8, 10, 0, 0, 0, 0, 0, 0, 0]);
	///     #    });
	/// }
	/// ```
	///
	/// Further properties of dispatchable functions are as follows:
	///
	/// - Unless if annotated by `dev_mode`, it must contain [`weight`] to denote the
	///   pre-dispatch weight consumed.
	/// - The dispatchable must declare its index via [`call_index`], which can override the
	///   position of a function in `enum Call`.
	/// - The first argument is always an `OriginFor` (or `T::RuntimeOrigin`).
	/// - The return type is always [`crate::dispatch::DispatchResult`] (or
	///   [`crate::dispatch::DispatchResultWithPostInfo`]).
	///
	/// **WARNING**: modifying dispatchables, changing their order (i.e. using [`call_index`]),
	/// removing some, etc., must be done with care. This will change the encoding of the call,
	/// and the call can be stored on-chain (e.g. in `pallet-scheduler`). Thus, migration
	/// might be needed. This is why the use of `call_index` is mandatory by default in FRAME.
	///
	/// ## Weight info
	///
	/// Each call needs to define a weight.
	/// * The weight can be defined explicitly using the attribute `#[pallet::weight($expr)]`
	///   (Note that argument of the call are available inside the expression).
	/// * Or it can be defined implicitly, the weight info for the calls needs to be specified
	///   in the call attribute: `#[pallet::call(weight = $WeightInfo)]`, then each call that
	///   doesn't have explicit weight will use `$WeightInfo::$call_name` as the weight.
	///
	/// * Or it can be simply ignored when the pallet is in `dev_mode`.
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///     use frame_support::pallet_prelude::*;
	///     use frame_system::pallet_prelude::*;
	///
	///     #[pallet::pallet]
	///     pub struct Pallet<T>(_);
	///
	///     #[pallet::config]
	///     pub trait Config: frame_system::Config {
	///         /// Type for specifying dispatchable weights.
	///         type WeightInfo: WeightInfo;
	///     }
	///
	///     /// The `WeightInfo` trait defines weight functions for dispatchable calls.
	///     pub trait WeightInfo {
	///         fn do_something() -> Weight;
	///         fn do_something_else() -> Weight;
	///     }
	///
	///     #[pallet::call(weight = <T as Config>::WeightInfo)]
	///     impl<T: Config> Pallet<T> {
	///         // Explicit weight definition using `#[pallet::weight(...)]`
	///         #[pallet::weight(<T as Config>::WeightInfo::do_something())]
	///         #[pallet::call_index(0)]
	///         pub fn do_something(
	///             origin: OriginFor<T>,
	///             foo: u32,
	///         ) -> DispatchResult {
	///             // Function logic here
	///             Ok(())
	///         }
	///
	///         // Implicit weight definition, the macro looks up to the weight info defined in
	///         // `#[pallet::call(weight = $WeightInfo)]` attribute. Then use
	///         // `$WeightInfo::do_something_else` as the weight function.
	///         #[pallet::call_index(1)]
	///         pub fn do_something_else(
	///             origin: OriginFor<T>,
	///             bar: u64,
	///         ) -> DispatchResult {
	///             // Function logic here
	///             Ok(())
	///         }
	///     }
	/// }
	/// ```
	///
	/// ## Default Behavior
	///
	/// If no `#[pallet::call]` exists, then a default implementation corresponding to the
	/// following code is automatically generated:
	///
	/// ```
	/// #[frame_support::pallet(dev_mode)]
	/// mod pallet {
	/// 	#[pallet::pallet]
	/// 	pub struct Pallet<T>(_);
	///
	/// 	#[pallet::call] // <- automatically generated
	/// 	impl<T: Config> Pallet<T> {} // <- automatically generated
	///
	/// 	#[pallet::config]
	/// 	pub trait Config: frame_system::Config {}
	/// }
	/// ```
	///
	/// ## Note on deprecation of Calls
	///
	/// - Usage of `deprecated` attribute will propagate deprecation information to the pallet
	///   metadata where the item was declared.
	/// - For general usage examples of `deprecated` attribute please refer to <https://doc.rust-lang.org/nightly/reference/attributes/diagnostics.html#the-deprecated-attribute>
	/// - Usage of `allow(deprecated)` on the item will propagate this attribute to the
	///   generated code.
	/// - If the item is annotated with `deprecated` attribute then the generated code will be
	///   automatically annotated with `allow(deprecated)`
	pub use frame_support_procedural::call;

	/// Enforce the index of a variant in the generated `enum Call`.
	///
	/// See [`call`] for more information.
	///
	/// All call indexes start from 0, until it encounters a dispatchable function with a
	/// defined call index. The dispatchable function that lexically follows the function with
	/// a defined call index will have that call index, but incremented by 1, e.g. if there are
	/// 3 dispatchable functions `fn foo`, `fn bar` and `fn qux` in that order, and only `fn
	/// bar` has a call index of 10, then `fn qux` will have an index of 11, instead of 1.
	pub use frame_support_procedural::call_index;

	/// Declares the arguments of a [`call`] function to be encoded using
	/// [`codec::Compact`].
	///
	/// This will results in smaller extrinsic encoding.
	///
	/// A common example of `compact` is for numeric values that are often times far far away
	/// from their theoretical maximum. For example, in the context of a crypto-currency, the
	/// balance of an individual account is oftentimes way less than what the numeric type
	/// allows. In all such cases, using `compact` is sensible.
	///
	/// ```
	/// #[frame_support::pallet(dev_mode)]
	/// pub mod custom_pallet {
	/// #   use frame_support::pallet_prelude::*;
	/// #   use frame_system::pallet_prelude::*;
	/// #   #[pallet::config]
	/// #   pub trait Config: frame_system::Config {}
	/// #   #[pallet::pallet]
	/// #   pub struct Pallet<T>(_);
	/// #   use frame_support::traits::BuildGenesisConfig;
	///     #[pallet::call]
	///     impl<T: Config> Pallet<T> {
	///         pub fn some_dispatchable(_origin: OriginFor<T>, #[pallet::compact] _input: u32) -> DispatchResult {
	///             Ok(())
	///         }
	///     }
	/// }
	pub use frame_support_procedural::compact;

	/// Allows you to define the genesis configuration for the pallet.
	///
	/// Item is defined as either an enum or a struct. It needs to be public and implement the
	/// trait [`frame_support::traits::BuildGenesisConfig`].
	///
	/// See [`genesis_build`] for an example.
	pub use frame_support_procedural::genesis_config;

	/// Allows you to define how the state of your pallet at genesis is built. This
	/// takes as input the `GenesisConfig` type (as `self`) and constructs the pallet's initial
	/// state.
	///
	/// The fields of the `GenesisConfig` can in turn be populated by the chain-spec.
	///
	/// ## Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// pub mod pallet {
	/// # 	#[pallet::config]
	/// # 	pub trait Config: frame_system::Config {}
	/// # 	#[pallet::pallet]
	/// # 	pub struct Pallet<T>(_);
	/// # 	use frame_support::traits::BuildGenesisConfig;
	///     #[pallet::genesis_config]
	///     #[derive(frame_support::DefaultNoBound)]
	///     pub struct GenesisConfig<T: Config> {
	///         foo: Vec<T::AccountId>
	///     }
	///
	///     #[pallet::genesis_build]
	///     impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
	///         fn build(&self) {
	///             // use &self to access fields.
	///             let foo = &self.foo;
	///             todo!()
	///         }
	///     }
	/// }
	/// ```
	pub use frame_support_procedural::genesis_build;

	/// Allows adding an associated type trait bounded by
	/// [`Get`](frame_support::pallet_prelude::Get) from [`pallet::config`](`macro@config`)
	/// into metadata.
	///
	/// ## Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///     use frame_support::pallet_prelude::*;
	///     # #[pallet::pallet]
	///     # pub struct Pallet<T>(_);
	///     #[pallet::config]
	///     pub trait Config: frame_system::Config {
	/// 		/// This is like a normal `Get` trait, but it will be added into metadata.
	/// 		#[pallet::constant]
	/// 		type Foo: Get<u32>;
	/// 	}
	/// }
	/// ```
	///
	/// ## Note on deprecation of constants
	///
	/// - Usage of `deprecated` attribute will propagate deprecation information to the pallet
	///   metadata where the item was declared.
	/// - For general usage examples of `deprecated` attribute please refer to <https://doc.rust-lang.org/nightly/reference/attributes/diagnostics.html#the-deprecated-attribute>
	/// - Usage of `allow(deprecated)` on the item will propagate this attribute to the
	///   generated code.
	/// - If the item is annotated with `deprecated` attribute then the generated code will be
	///   automatically annotated with `allow(deprecated)`
	pub use frame_support_procedural::constant;

	/// Declares a type alias as a storage item.
	///
	/// Storage items are pointers to data stored on-chain (the *blockchain state*), under a
	/// specific key. The exact key is dependent on the type of the storage.
	///
	/// > From the perspective of this pallet, the entire blockchain state is abstracted behind
	/// > a key-value api, namely [`sp_io::storage`].
	///
	/// ## Storage Types
	///
	/// The following storage types are supported by the `#[storage]` macro. For specific
	/// information about each storage type, refer to the documentation of the respective type.
	///
	/// * [`StorageValue`](crate::storage::types::StorageValue)
	/// * [`StorageMap`](crate::storage::types::StorageMap)
	/// * [`CountedStorageMap`](crate::storage::types::CountedStorageMap)
	/// * [`StorageDoubleMap`](crate::storage::types::StorageDoubleMap)
	/// * [`StorageNMap`](crate::storage::types::StorageNMap)
	/// * [`CountedStorageNMap`](crate::storage::types::CountedStorageNMap)
	///
	/// ## Storage Type Usage
	///
	/// The following details are relevant to all of the aforementioned storage types.
	/// Depending on the exact storage type, it may require the following generic parameters:
	///
	/// * [`Prefix`](#prefixes) - Used to give the storage item a unique key in the underlying
	///   storage.
	/// * `Key` - Type of the keys used to store the values,
	/// * `Value` - Type of the value being stored,
	/// * [`Hasher`](#hashers) - Used to ensure the keys of a map are uniformly distributed,
	/// * [`QueryKind`](#querykind) - Used to configure how to handle queries to the underlying
	///   storage,
	/// * `OnEmpty` - Used to handle missing values when querying the underlying storage,
	/// * `MaxValues` - _not currently used_.
	///
	/// Each `Key` type requires its own designated `Hasher` declaration, so that
	/// [`StorageDoubleMap`](frame_support::storage::types::StorageDoubleMap) needs two of
	/// each, and [`StorageNMap`](frame_support::storage::types::StorageNMap) needs `N` such
	/// pairs. Since [`StorageValue`](frame_support::storage::types::StorageValue) only stores
	/// a single element, no configuration of hashers is needed.
	///
	/// ### Syntax
	///
	/// Two general syntaxes are supported, as demonstrated below:
	///
	/// 1. Named type parameters, e.g., `type Foo<T> = StorageValue<Value = u32>`.
	/// 2. Positional type parameters, e.g., `type Foo<T> = StorageValue<_, u32>`.
	///
	/// In both instances, declaring the generic parameter `<T>` is mandatory. Optionally, it
	/// can also be explicitly declared as `<T: Config>`. In the compiled code, `T` will
	/// automatically include the trait bound `Config`.
	///
	/// Note that in positional syntax, the first generic type parameter must be `_`.
	///
	/// #### Example
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///     # use frame_support::pallet_prelude::*;
	///     # #[pallet::config]
	///     # pub trait Config: frame_system::Config {}
	///     # #[pallet::pallet]
	///     # pub struct Pallet<T>(_);
	///     /// Positional syntax, without bounding `T`.
	///     #[pallet::storage]
	///     pub type Foo<T> = StorageValue<_, u32>;
	///
	///     /// Positional syntax, with bounding `T`.
	///     #[pallet::storage]
	///     pub type Bar<T: Config> = StorageValue<_, u32>;
	///
	///     /// Named syntax.
	///     #[pallet::storage]
	///     pub type Baz<T> = StorageMap<Hasher = Blake2_128Concat, Key = u32, Value = u32>;
	/// }
	/// ```
	///
	/// ### Value Trait Bounds
	///
	/// To use a type as the value of a storage type, be it `StorageValue`, `StorageMap` or
	/// anything else, you need to meet a number of trait bound constraints.
	///
	/// See: <https://paritytech.github.io/polkadot-sdk/master/polkadot_sdk_docs/reference_docs/frame_storage_derives/index.html>.
	///
	/// Notably, all value types need to implement `Encode`, `Decode`, `MaxEncodedLen` and
	/// `TypeInfo`, and possibly `Default`, if
	/// [`ValueQuery`](frame_support::storage::types::ValueQuery) is used, explained in the
	/// next section.
	///
	/// ### QueryKind
	///
	/// Every storage type mentioned above has a generic type called
	/// [`QueryKind`](frame_support::storage::types::QueryKindTrait) that determines its
	/// "query" type. This refers to the kind of value returned when querying the storage, for
	/// instance, through a `::get()` method.
	///
	/// There are three types of queries:
	///
	/// 1. [`OptionQuery`](frame_support::storage::types::OptionQuery): The default query type.
	///    It returns `Some(V)` if the value is present, or `None` if it isn't, where `V` is
	///    the value type.
	/// 2. [`ValueQuery`](frame_support::storage::types::ValueQuery): Returns the value itself
	///    if present; otherwise, it returns `Default::default()`. This behavior can be
	///    adjusted with the `OnEmpty` generic parameter, which defaults to `OnEmpty =
	///    GetDefault`.
	/// 3. [`ResultQuery`](frame_support::storage::types::ResultQuery): Returns `Result<V, E>`,
	///    where `V` is the value type.
	///
	/// See [`QueryKind`](frame_support::storage::types::QueryKindTrait) for further examples.
	///
	/// ### Optimized Appending
	///
	/// All storage items — such as
	/// [`StorageValue`](frame_support::storage::types::StorageValue),
	/// [`StorageMap`](frame_support::storage::types::StorageMap), and their variants—offer an
	/// `::append()` method optimized for collections. Using this method avoids the
	/// inefficiency of decoding and re-encoding entire collections when adding items. For
	/// instance, consider the storage declaration `type MyVal<T> = StorageValue<_, Vec<u8>,
	/// ValueQuery>`. With `MyVal` storing a large list of bytes, `::append()` lets you
	/// directly add bytes to the end in storage without processing the full list. Depending on
	/// the storage type, additional key specifications may be needed.
	///
	/// #### Example
	#[doc = docify::embed!("src/lib.rs", example_storage_value_append)]
	/// Similarly, there also exists a `::try_append()` method, which can be used when handling
	/// types where an append operation might fail, such as a
	/// [`BoundedVec`](frame_support::BoundedVec).
	///
	/// #### Example
	#[doc = docify::embed!("src/lib.rs", example_storage_value_try_append)]
	/// ### Optimized Length Decoding
	///
	/// All storage items — such as
	/// [`StorageValue`](frame_support::storage::types::StorageValue),
	/// [`StorageMap`](frame_support::storage::types::StorageMap), and their counterparts —
	/// incorporate the `::decode_len()` method. This method allows for efficient retrieval of
	/// a collection's length without the necessity of decoding the entire dataset.
	/// #### Example
	#[doc = docify::embed!("src/lib.rs", example_storage_value_decode_len)]
	/// ### Hashers
	///
	/// For all storage types, except
	/// [`StorageValue`](frame_support::storage::types::StorageValue), a set of hashers needs
	/// to be specified. The choice of hashers is crucial, especially in production chains. The
	/// purpose of storage hashers in maps is to ensure the keys of a map are
	/// uniformly distributed. An unbalanced map/trie can lead to inefficient performance.
	///
	/// In general, hashers are categorized as either cryptographically secure or not. The
	/// former is slower than the latter. `Blake2` and `Twox` serve as examples of each,
	/// respectively.
	///
	/// As a rule of thumb:
	///
	/// 1. If the map keys are not controlled by end users, or are cryptographically secure by
	/// definition (e.g., `AccountId`), then the use of cryptographically secure hashers is NOT
	/// required.
	/// 2. If the map keys are controllable by the end users, cryptographically secure hashers
	/// should be used.
	///
	/// For more information, look at the types that implement
	/// [`frame_support::StorageHasher`](frame_support::StorageHasher).
	///
	/// Lastly, it's recommended for hashers with "concat" to have reversible hashes. Refer to
	/// the implementors section of
	/// [`hash::ReversibleStorageHasher`](frame_support::hash::ReversibleStorageHasher).
	///
	/// ### Prefixes
	///
	/// Internally, every storage type generates a "prefix". This prefix serves as the initial
	/// segment of the key utilized to store values in the on-chain state (i.e., the final key
	/// used in [`sp_io::storage`](sp_io::storage)). For all storage types, the following rule
	/// applies:
	///
	/// > The storage prefix begins with `twox128(pallet_prefix) ++ twox128(STORAGE_PREFIX)`,
	/// > where
	/// > `pallet_prefix` is the name assigned to the pallet instance in
	/// > [`frame_support::construct_runtime`](frame_support::construct_runtime), and
	/// > `STORAGE_PREFIX` is the name of the `type` aliased to a particular storage type, such
	/// > as
	/// > `Foo` in `type Foo<T> = StorageValue<..>`.
	///
	/// For [`StorageValue`](frame_support::storage::types::StorageValue), no additional key is
	/// required. For map types, the prefix is extended with one or more keys defined by the
	/// map.
	///
	/// #### Example
	#[doc = docify::embed!("src/lib.rs", example_storage_value_map_prefixes)]
	/// ## Related Macros
	///
	/// The following attribute macros can be used in conjunction with the `#[storage]` macro:
	///
	/// * [`macro@getter`]: Creates a custom getter function.
	/// * [`macro@storage_prefix`]: Overrides the default prefix of the storage item.
	/// * [`macro@unbounded`]: Declares the storage item as unbounded.
	/// * [`macro@disable_try_decode_storage`]: Declares that try-runtime checks should not
	///   attempt to decode the storage item.
	///
	/// #### Example
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///     # use frame_support::pallet_prelude::*;
	///     # #[pallet::config]
	///     # pub trait Config: frame_system::Config {}
	///     # #[pallet::pallet]
	///     # pub struct Pallet<T>(_);
	/// 	/// A kitchen-sink StorageValue, with all possible additional attributes.
	///     #[pallet::storage]
	/// 	#[pallet::getter(fn foo)]
	/// 	#[pallet::storage_prefix = "OtherFoo"]
	/// 	#[pallet::unbounded]
	/// 	#[pallet::disable_try_decode_storage]
	///     pub type Foo<T> = StorageValue<_, u32, ValueQuery>;
	/// }
	/// ```
	///
	/// ## Note on deprecation of storage items
	///
	/// - Usage of `deprecated` attribute will propagate deprecation information to the pallet
	///   metadata where the storage item was declared.
	/// - For general usage examples of `deprecated` attribute please refer to <https://doc.rust-lang.org/nightly/reference/attributes/diagnostics.html#the-deprecated-attribute>
	/// - Usage of `allow(deprecated)` on the item will propagate this attribute to the
	///   generated code.
	/// - If the item is annotated with `deprecated` attribute then the generated code will be
	///   automatically annotated with `allow(deprecated)`
	pub use frame_support_procedural::storage;

	pub use frame_support_procedural::{
		authorize, task_condition, task_index, task_list, task_weight, tasks_experimental,
		weight_of_authorize,
	};

	/// Allows a pallet to declare a type as an origin.
	///
	/// If defined as such, this type will be amalgamated at the runtime level into
	/// `RuntimeOrigin`, very similar to [`call`], [`error`] and [`event`]. See
	/// [`composite_enum`] for similar cases.
	///
	/// Origin is a complex FRAME topics and is further explained in `polkadot_sdk_docs`.
	///
	/// ## Syntax Variants
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///     # use frame_support::pallet_prelude::*;
	///     # #[pallet::config]
	///     # pub trait Config: frame_system::Config {}
	///     # #[pallet::pallet]
	///     # pub struct Pallet<T>(_);
	/// 	/// On the spot declaration.
	///     #[pallet::origin]
	/// 	#[derive(PartialEq, Eq, Clone, Debug, Encode, Decode, TypeInfo, MaxEncodedLen)]
	/// 	pub enum Origin {
	/// 		Foo,
	/// 		Bar,
	/// 	}
	/// }
	/// ```
	///
	/// Or, more commonly used:
	///
	/// ```
	/// #[frame_support::pallet]
	/// mod pallet {
	///     # use frame_support::pallet_prelude::*;
	///     # #[pallet::config]
	///     # pub trait Config: frame_system::Config {}
	///     # #[pallet::pallet]
	///     # pub struct Pallet<T>(_);
	/// 	#[derive(PartialEq, Eq, Clone, Debug, Encode, Decode, TypeInfo, MaxEncodedLen)]
	/// 	pub enum RawOrigin {
	/// 		Foo,
	/// 		Bar,
	/// 	}
	///
	/// 	#[pallet::origin]
	/// 	pub type Origin = RawOrigin;
	/// }
	/// ```
	///
	/// ## Warning
	///
	/// Modifying any pallet's origin type will cause the runtime level origin type to also
	/// change in encoding. If stored anywhere on-chain, this will require a data migration.
	///
	/// Read more about origins at the [Origin Reference
	/// Docs](../../polkadot_sdk_docs/reference_docs/frame_origin/index.html).
	pub use frame_support_procedural::origin;
}

#[doc(inline)]
pub use frame_support_procedural::register_default_impl;

// Generate a macro that will enable/disable code based on `std` feature being active.
sp_core::generate_feature_enabled_macro!(std_enabled, feature = "std", $);
// Generate a macro that will enable/disable code based on `try-runtime` feature being active.
sp_core::generate_feature_enabled_macro!(try_runtime_enabled, feature = "try-runtime", $);
sp_core::generate_feature_enabled_macro!(try_runtime_or_std_enabled, any(feature = "try-runtime", feature = "std"), $);
sp_core::generate_feature_enabled_macro!(try_runtime_and_std_not_enabled, all(not(feature = "try-runtime"), not(feature = "std")), $);

/// Helper for implementing GenesisBuilder runtime API
pub mod genesis_builder_helper;

/// Helper for generating the `RuntimeGenesisConfig` instance for presets.
pub mod generate_genesis_config;

#[cfg(test)]
mod test {
	// use super::*;
	use crate::{
		hash::*,
		storage::types::{StorageMap, StorageValue, ValueQuery},
		traits::{ConstU32, StorageInstance},
		BoundedVec,
	};
	use sp_io::{hashing::twox_128, TestExternalities};

	struct Prefix;
	impl StorageInstance for Prefix {
		fn pallet_prefix() -> &'static str {
			"test"
		}
		const STORAGE_PREFIX: &'static str = "foo";
	}

	struct Prefix1;
	impl StorageInstance for Prefix1 {
		fn pallet_prefix() -> &'static str {
			"test"
		}
		const STORAGE_PREFIX: &'static str = "MyVal";
	}
	struct Prefix2;
	impl StorageInstance for Prefix2 {
		fn pallet_prefix() -> &'static str {
			"test"
		}
		const STORAGE_PREFIX: &'static str = "MyMap";
	}

	#[docify::export]
	#[test]
	pub fn example_storage_value_try_append() {
		type MyVal = StorageValue<Prefix, BoundedVec<u8, ConstU32<10>>, ValueQuery>;

		TestExternalities::default().execute_with(|| {
			MyVal::set(BoundedVec::try_from(vec![42, 43]).unwrap());
			assert_eq!(MyVal::get(), vec![42, 43]);
			// Try to append a single u32 to BoundedVec stored in `MyVal`
			crate::assert_ok!(MyVal::try_append(40));
			assert_eq!(MyVal::get(), vec![42, 43, 40]);
		});
	}

	#[docify::export]
	#[test]
	pub fn example_storage_value_append() {
		type MyVal = StorageValue<Prefix, Vec<u8>, ValueQuery>;

		TestExternalities::default().execute_with(|| {
			MyVal::set(vec![42, 43]);
			assert_eq!(MyVal::get(), vec![42, 43]);
			// Append a single u32 to Vec stored in `MyVal`
			MyVal::append(40);
			assert_eq!(MyVal::get(), vec![42, 43, 40]);
		});
	}

	#[docify::export]
	#[test]
	pub fn example_storage_value_decode_len() {
		type MyVal = StorageValue<Prefix, BoundedVec<u8, ConstU32<10>>, ValueQuery>;

		TestExternalities::default().execute_with(|| {
			MyVal::set(BoundedVec::try_from(vec![42, 43]).unwrap());
			assert_eq!(MyVal::decode_len().unwrap(), 2);
		});
	}

	#[docify::export]
	#[test]
	pub fn example_storage_value_map_prefixes() {
		type MyVal = StorageValue<Prefix1, u32, ValueQuery>;
		type MyMap = StorageMap<Prefix2, Blake2_128Concat, u16, u32, ValueQuery>;
		TestExternalities::default().execute_with(|| {
			// This example assumes `pallet_prefix` to be "test"
			// Get storage key for `MyVal` StorageValue
			assert_eq!(
				MyVal::hashed_key().to_vec(),
				[twox_128(b"test"), twox_128(b"MyVal")].concat()
			);
			// Get storage key for `MyMap` StorageMap and `key` = 1
			let mut k: Vec<u8> = vec![];
			k.extend(&twox_128(b"test"));
			k.extend(&twox_128(b"MyMap"));
			k.extend(&1u16.blake2_128_concat());
			assert_eq!(MyMap::hashed_key_for(1).to_vec(), k);
		});
	}
}
