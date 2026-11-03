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

//! Proc macro of Support code for the runtime.

#![recursion_limit = "512"]
#![deny(rustdoc::broken_intra_doc_links)]

mod benchmark;
mod construct_runtime;
mod crate_version;
mod deprecation;
mod derive_impl;
mod dummy_part_checker;
mod dynamic_params;
mod key_prefix;
mod match_and_insert;
mod no_bound;
mod pallet;
mod pallet_error;
mod runtime;
mod storage_alias;
mod stored;
mod transactional;
mod tt_macro;

use frame_support_procedural_tools::generate_access_from_frame_or_crate;
use macro_magic::{import_tokens_attr, import_tokens_attr_verbatim};
use proc_macro::TokenStream;
use quote::{quote, ToTokens};
use std::{cell::RefCell, str::FromStr};
use syn::{parse_macro_input, Error, ItemImpl, ItemMod, TraitItemType};

pub(crate) const INHERENT_INSTANCE_NAME: &str = "__InherentHiddenInstance";

thread_local! {
	/// A global counter, can be used to generate a relatively unique identifier.
	static COUNTER: RefCell<Counter> = RefCell::new(Counter(0));
}

/// Counter to generate a relatively unique identifier for macros. This is necessary because
/// declarative macros gets hoisted to the crate root, which shares the namespace with other pallets
/// containing the very same macros.
struct Counter(u64);

impl Counter {
	fn inc(&mut self) -> u64 {
		let ret = self.0;
		self.0 += 1;
		ret
	}
}

/// Get the value from the given environment variable set by cargo.
///
/// The value is parsed into the requested destination type.
fn get_cargo_env_var<T: FromStr>(version_env: &str) -> std::result::Result<T, ()> {
	let version = std::env::var(version_env)
		.unwrap_or_else(|_| panic!("`{}` is always set by cargo; qed", version_env));

	T::from_str(&version).map_err(drop)
}

/// Generate the counter_prefix related to the storage.
/// counter_prefix is used by counted storage map.
fn counter_prefix(prefix: &str) -> String {
	format!("CounterFor{}", prefix)
}

/// Construct a runtime, with the given name and the given pallets.
///
/// NOTE: A new version of this macro is available at `frame_support::runtime`. This macro will
/// soon be deprecated. Please use the new macro instead.
///
/// The parameters here are specific types for `Block`, `NodeBlock`, and `UncheckedExtrinsic`
/// and the pallets that are used by the runtime.
/// `Block` is the block type that is used in the runtime and `NodeBlock` is the block type
/// that is used in the node. For instance they can differ in the extrinsics type.
///
/// # Example:
///
/// ```ignore
/// construct_runtime!(
///     pub enum Runtime where
///         Block = Block,
///         NodeBlock = node::Block,
///         UncheckedExtrinsic = UncheckedExtrinsic
///     {
///         System: frame_system::{Pallet, Call, Event<T>, Config<T>} = 0,
///         Test: path::to::test::{Pallet, Call} = 1,
///
///         // Pallets with instances.
///         Test2_Instance1: test2::<Instance1>::{Pallet, Call, Storage, Event<T, I>, Config<T, I>, Origin<T, I>},
///         Test2_DefaultInstance: test2::{Pallet, Call, Storage, Event<T>, Config<T>, Origin<T>} = 4,
///
///         // Pallets declared with `pallet` attribute macro: no need to define the parts
///         Test3_Instance1: test3::<Instance1>,
///         Test3_DefaultInstance: test3,
///
///         // with `exclude_parts` keyword some part can be excluded.
///         Test4_Instance1: test4::<Instance1> exclude_parts { Call, Origin },
///         Test4_DefaultInstance: test4 exclude_parts { Storage },
///
///         // with `use_parts` keyword, a subset of the pallet parts can be specified.
///         Test4_Instance1: test4::<Instance1> use_parts { Pallet, Call},
///         Test4_DefaultInstance: test4 use_parts { Pallet },
///     }
/// )
/// ```
///
/// Each pallet is declared as such:
/// * `Identifier`: name given to the pallet that uniquely identifies it.
///
/// * `:`: colon separator
///
/// * `path::to::pallet`: identifiers separated by colons which declare the path to a pallet
///   definition.
///
/// * `::<InstanceN>` optional: specify the instance of the pallet to use. If not specified it will
///   use the default instance (or the only instance in case of non-instantiable pallets).
///
/// * `::{ Part1, Part2<T>, .. }` optional if pallet declared with `frame_support::pallet`: Comma
///   separated parts declared with their generic. If a pallet is declared with
///   `frame_support::pallet` macro then the parts can be automatically derived if not explicitly
///   provided. We provide support for the following module parts in a pallet:
///
///   - `Pallet` - Required for all pallets
///   - `Call` - If the pallet has callable functions
///   - `Storage` - If the pallet uses storage
///   - `Event` or `Event<T>` (if the event is generic) - If the pallet emits events
///   - `Origin` or `Origin<T>` (if the origin is generic) - If the pallet has instantiable origins
///   - `Config` or `Config<T>` (if the config is generic) - If the pallet builds the genesis
///     storage with `GenesisConfig`
///   - `Inherent` - If the pallet provides/can check inherents.
///   - `ValidateUnsigned` - If the pallet validates unsigned extrinsics.
///
///   It is important to list these parts here to export them correctly in the metadata or to make
/// the pallet usable in the runtime.
///
/// * `exclude_parts { Part1, Part2 }` optional: comma separated parts without generics. I.e. one of
///   `Pallet`, `Call`, `Storage`, `Event`, `Origin`, `Config`, `Inherent`, `ValidateUnsigned`. It
///   is incompatible with `use_parts`. This specifies the part to exclude. In order to select
///   subset of the pallet parts.
///
///   For example excluding the part `Call` can be useful if the runtime doesn't want to make the
///   pallet calls available.
///
/// * `use_parts { Part1, Part2 }` optional: comma separated parts without generics. I.e. one of
///   `Pallet`, `Call`, `Storage`, `Event`, `Origin`, `Config`, `Inherent`, `ValidateUnsigned`. It
///   is incompatible with `exclude_parts`. This specifies the part to use. In order to select a
///   subset of the pallet parts.
///
///   For example not using the part `Call` can be useful if the runtime doesn't want to make the
///   pallet calls available.
///
/// * `= $n` optional: number to define at which index the pallet variants in `OriginCaller`, `Call`
///   and `Event` are encoded, and to define the ModuleToIndex value.
///
///   if `= $n` is not given, then index is resolved in the same way as fieldless enum in Rust
///   (i.e. incrementally from previous index):
///   ```nocompile
///   pallet1 .. = 2,
///   pallet2 .., // Here pallet2 is given index 3
///   pallet3 .. = 0,
///   pallet4 .., // Here pallet4 is given index 1
///   ```
///
/// # Note
///
/// The population of the genesis storage depends on the order of pallets. So, if one of your
/// pallets depends on another pallet, the pallet that is depended upon needs to come before
/// the pallet depending on it.
///
/// # Type definitions
///
/// * The macro generates a type alias for each pallet to their `Pallet`. E.g. `type System =
///   frame_system::Pallet<Runtime>`
#[proc_macro]
pub fn construct_runtime(input: TokenStream) -> TokenStream {
	construct_runtime::construct_runtime(input)
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet`.
#[proc_macro_attribute]
pub fn pallet(attr: TokenStream, item: TokenStream) -> TokenStream {
	pallet::pallet(attr, item)
}

/// An attribute macro that can be attached to a (non-empty) module declaration. Doing so will
/// designate that module as a benchmarking module.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn benchmarks(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	match benchmark::benchmarks(attr, tokens, false) {
		Ok(tokens) => tokens,
		Err(err) => err.to_compile_error().into(),
	}
}

/// An attribute macro that can be attached to a (non-empty) module declaration. Doing so will
/// designate that module as an instance benchmarking module.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn instance_benchmarks(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	match benchmark::benchmarks(attr, tokens, true) {
		Ok(tokens) => tokens,
		Err(err) => err.to_compile_error().into(),
	}
}

/// An attribute macro used to declare a benchmark within a benchmarking module. Must be
/// attached to a function definition containing an `#[extrinsic_call]` or `#[block]`
/// attribute.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn benchmark(_attrs: TokenStream, _tokens: TokenStream) -> TokenStream {
	quote!(compile_error!(
		"`#[benchmark]` must be in a module labeled with #[benchmarks] or #[instance_benchmarks]."
	))
	.into()
}

/// An attribute macro used to specify the extrinsic call inside a benchmark function, and also
/// used as a boundary designating where the benchmark setup code ends, and the benchmark
/// verification code begins.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn extrinsic_call(_attrs: TokenStream, _tokens: TokenStream) -> TokenStream {
	quote!(compile_error!(
		"`#[extrinsic_call]` must be in a benchmark function definition labeled with `#[benchmark]`."
	);)
	.into()
}

/// An attribute macro used to specify that a block should be the measured portion of the
/// enclosing benchmark function, This attribute is also used as a boundary designating where
/// the benchmark setup code ends, and the benchmark verification code begins.
///
/// See `frame_benchmarking::v2` for more info.
#[proc_macro_attribute]
pub fn block(_attrs: TokenStream, _tokens: TokenStream) -> TokenStream {
	quote!(compile_error!(
		"`#[block]` must be in a benchmark function definition labeled with `#[benchmark]`."
	))
	.into()
}

/// Execute the annotated function in a new storage transaction.
///
/// The return type of the annotated function must be `Result`. All changes to storage performed
/// by the annotated function are discarded if it returns `Err`, or committed if `Ok`.
///
/// # Example
///
/// ```nocompile
/// #[transactional]
/// fn value_commits(v: u32) -> result::Result<u32, &'static str> {
/// 	Value::set(v);
/// 	Ok(v)
/// }
///
/// #[transactional]
/// fn value_rollbacks(v: u32) -> result::Result<u32, &'static str> {
/// 	Value::set(v);
/// 	Err("nah")
/// }
/// ```
#[proc_macro_attribute]
pub fn transactional(attr: TokenStream, input: TokenStream) -> TokenStream {
	transactional::transactional(attr, input).unwrap_or_else(|e| e.to_compile_error().into())
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::require_transactional`.
#[proc_macro_attribute]
pub fn require_transactional(attr: TokenStream, input: TokenStream) -> TokenStream {
	transactional::require_transactional(attr, input)
		.unwrap_or_else(|e| e.to_compile_error().into())
}

/// Derive [`Clone`] but do not bound any generic.
///
/// Docs at `frame_support::CloneNoBound`.
#[proc_macro_derive(CloneNoBound)]
pub fn derive_clone_no_bound(input: TokenStream) -> TokenStream {
	no_bound::clone::derive_clone_no_bound(input)
}

/// Derive [`Debug`] but do not bound any generics.
///
/// Docs at `frame_support::DebugNoBound`.
#[proc_macro_derive(DebugNoBound)]
pub fn derive_debug_no_bound(input: TokenStream) -> TokenStream {
	no_bound::debug::derive_debug_no_bound(input)
}

/// Derive [`PartialEq`] but do not bound any generic.
///
/// Docs at `frame_support::PartialEqNoBound`.
#[proc_macro_derive(PartialEqNoBound)]
pub fn derive_partial_eq_no_bound(input: TokenStream) -> TokenStream {
	no_bound::partial_eq::derive_partial_eq_no_bound(input)
}

/// DeriveEq but do no bound any generic.
///
/// Docs at `frame_support::EqNoBound`.
#[proc_macro_derive(EqNoBound)]
pub fn derive_eq_no_bound(input: TokenStream) -> TokenStream {
	let input = syn::parse_macro_input!(input as syn::DeriveInput);

	let name = &input.ident;
	let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

	quote::quote_spanned!(name.span() =>
		#[allow(deprecated)]
		const _: () = {
			impl #impl_generics ::core::cmp::Eq for #name #ty_generics #where_clause {}
		};
	)
	.into()
}

/// Derive [`PartialOrd`] but do not bound any generic. Docs are at
/// `frame_support::PartialOrdNoBound`.
#[proc_macro_derive(PartialOrdNoBound)]
pub fn derive_partial_ord_no_bound(input: TokenStream) -> TokenStream {
	no_bound::partial_ord::derive_partial_ord_no_bound(input)
}

/// Derive [`Ord`] but do no bound any generic. Docs are at `frame_support::OrdNoBound`.
#[proc_macro_derive(OrdNoBound)]
pub fn derive_ord_no_bound(input: TokenStream) -> TokenStream {
	no_bound::ord::derive_ord_no_bound(input)
}

/// derive `Default` but do no bound any generic. Docs are at `frame_support::DefaultNoBound`.
#[proc_macro_derive(DefaultNoBound, attributes(default))]
pub fn derive_default_no_bound(input: TokenStream) -> TokenStream {
	no_bound::default::derive_default_no_bound(input)
}

/// Macro used internally in FRAME to generate the crate version for a pallet.
#[proc_macro]
pub fn crate_to_crate_version(input: TokenStream) -> TokenStream {
	crate_version::crate_to_crate_version(input)
		.unwrap_or_else(|e| e.to_compile_error())
		.into()
}

/// The number of module instances supported by the runtime, starting at index 1,
/// and up to `NUMBER_OF_INSTANCE`.
pub(crate) const NUMBER_OF_INSTANCE: u8 = 16;

/// This macro is meant to be used by frame-support only.
/// It implements the trait `HasKeyPrefix` and `HasReversibleKeyPrefix` for tuple of `Key`.
#[proc_macro]
pub fn impl_key_prefix_for_tuples(input: TokenStream) -> TokenStream {
	key_prefix::impl_key_prefix_for_tuples(input)
		.unwrap_or_else(syn::Error::into_compile_error)
		.into()
}

/// Internal macro use by frame_support to generate dummy part checker for old pallet declaration
#[proc_macro]
pub fn __generate_dummy_part_checker(input: TokenStream) -> TokenStream {
	dummy_part_checker::generate_dummy_part_checker(input)
}

/// Macro that inserts some tokens after the first match of some pattern.
///
/// # Example:
///
/// ```nocompile
/// match_and_insert!(
///     target = [{ Some content with { at some point match pattern } other match pattern are ignored }]
///     pattern = [{ match pattern }] // the match pattern cannot contain any group: `[]`, `()`, `{}`
/// 								  // can relax this constraint, but will require modifying the match logic in code
///     tokens = [{ expansion tokens }] // content inside braces can be anything including groups
/// );
/// ```
///
/// will generate:
///
/// ```nocompile
///     Some content with { at some point match pattern expansion tokens } other match patterns are
///     ignored
/// ```
#[proc_macro]
pub fn match_and_insert(input: TokenStream) -> TokenStream {
	match_and_insert::match_and_insert(input)
}

#[proc_macro_derive(PalletError, attributes(codec))]
pub fn derive_pallet_error(input: TokenStream) -> TokenStream {
	pallet_error::derive_pallet_error(input)
}

/// Internal macro used by `frame_support` to create tt-call-compliant macros
#[proc_macro]
pub fn __create_tt_macro(input: TokenStream) -> TokenStream {
	tt_macro::create_tt_return_macro(input)
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::storage_alias`.
#[proc_macro_attribute]
pub fn storage_alias(attributes: TokenStream, input: TokenStream) -> TokenStream {
	storage_alias::storage_alias(attributes.into(), input.into())
		.unwrap_or_else(|r| r.into_compile_error())
		.into()
}

/// Attribute macro for simplifying storage type definitions with consistent field-based bounding.
///
/// Derives the implementation of `Encode`, `Decode`, `DecodeWithMemTracking`, `MaxEncodedLen`,
/// `Clone`, `PartialEq`, `Eq`, `Debug` and `TypeInfo.
///
/// Automatically extracts field types and applies derives with bounds on those fields, ensuring
/// consistent behavior across all traits. Supports both structs and enums.
///
/// Directly recursive types are not supported.
///
/// # Example
///
/// ```ignore
/// #[frame_support::stored]
/// pub struct Foo<F, F2> {
///     f: F,
///     f2: Vec<F2>,
/// }
/// ```
///
/// In this example, the macro will automatically apply field-based bounds to `F` and `F2`
/// (requiring them to implement `Clone`, `Eq`, `PartialEq`, `Debug`, `TypeInfo`, `Codec`, etc.)
/// without requiring the user to manually specify them on the generic parameters.
///
/// For pallet storage, you can of course still use generics, in this example bound `T::Balance`
/// and not `T` as the bounds are applied to the fields.
///
/// ```ignore
/// # trait ABCD {
/// #     type Balance;
/// # }
/// #[frame_support::stored]
/// pub struct AccountData<T: ABCD> {
///     pub free: T::Balance,
///     pub reserved: T::Balance,
/// }
/// ```
///
/// By default the type params are skipped, because they are rarely used. But to not skip them
/// an attribute can used as follows:
/// ```ignore
/// #[frame_support::stored(no_skip_type_params)]
/// pub struct Bar<T>(T);
/// ```
#[proc_macro_attribute]
pub fn stored(attr: TokenStream, item: TokenStream) -> TokenStream {
	stored::stored(attr, item)
}

/// This attribute can be used to derive a full implementation of a trait based on a local partial
/// impl and an external impl containing defaults that can be overridden in the local impl.
///
/// For a full end-to-end example, see [below](#use-case-auto-derive-test-pallet-config-traits).
///
/// # Usage
///
/// The attribute should be attached to an impl block (strictly speaking a `syn::ItemImpl`) for
/// which we want to inject defaults in the event of missing trait items in the block.
///
/// The attribute minimally takes a single `default_impl_path` argument, which should be the module
/// path to an impl registered via [`#[register_default_impl]`](`macro@register_default_impl`) that
/// contains the default trait items we want to potentially inject, with the general form:
///
/// ```ignore
/// #[derive_impl(default_impl_path)]
/// impl SomeTrait for SomeStruct {
///     ...
/// }
/// ```
///
/// Optionally, a `disambiguation_path` can be specified as follows by providing `as path::here`
/// after the `default_impl_path`:
///
/// ```ignore
/// #[derive_impl(default_impl_path as disambiguation_path)]
/// impl SomeTrait for SomeStruct {
///     ...
/// }
/// ```
///
/// The `disambiguation_path`, if specified, should be the path to a trait that will be used to
/// qualify all default entries that are injected into the local impl. For example if your
/// `default_impl_path` is `some::path::TestTraitImpl` and your `disambiguation_path` is
/// `another::path::DefaultTrait`, any items injected into the local impl will be qualified as
/// `<some::path::TestTraitImpl as another::path::DefaultTrait>::specific_trait_item`.
///
/// If you omit the `as disambiguation_path` portion, the `disambiguation_path` will internally
/// default to `A` from the `impl A for B` part of the default impl. This is useful for scenarios
/// where all of the relevant types are already in scope via `use` statements.
///
/// In case the `default_impl_path` is scoped to a different module such as
/// `some::path::TestTraitImpl`, the same scope is assumed for the `disambiguation_path`, i.e.
/// `some::A`. This enables the use of `derive_impl` attribute without having to specify the
/// `disambiguation_path` in most (if not all) uses within FRAME's context.
///
/// Conversely, the `default_impl_path` argument is required and cannot be omitted.
///
/// Optionally, `no_aggregated_types` can be specified as follows:
///
/// ```ignore
/// #[derive_impl(default_impl_path as disambiguation_path, no_aggregated_types)]
/// impl SomeTrait for SomeStruct {
///     ...
/// }
/// ```
///
/// If specified, this indicates that the aggregated types (as denoted by impl items
/// attached with [`#[inject_runtime_type]`]) should not be injected with the respective concrete
/// types. By default, all such types are injected.
///
/// You can also make use of `#[pallet::no_default]` on specific items in your default impl that you
/// want to ensure will not be copied over but that you nonetheless want to use locally in the
/// context of the foreign impl and the pallet (or context) in which it is defined.
///
/// ## Use-Case Example: Auto-Derive Test Pallet Config Traits
///
/// The `#[derive_imp(..)]` attribute can be used to derive a test pallet `Config` based on an
/// existing pallet `Config` that has been marked with
/// [`#[pallet::config(with_default)]`](`macro@config`) (which under the hood, generates a
/// `DefaultConfig` trait in the pallet in which the macro was invoked).
///
/// In this case, the `#[derive_impl(..)]` attribute should be attached to an `impl` block that
/// implements a compatible `Config` such as `frame_system::Config` for a test/mock runtime, and
/// should receive as its first argument the path to a `DefaultConfig` impl that has been registered
/// via [`#[register_default_impl]`](`macro@register_default_impl`), and as its second argument, the
/// path to the auto-generated `DefaultConfig` for the existing pallet `Config` we want to base our
/// test config off of.
///
/// The following is what the `basic` example pallet would look like with a default testing config:
///
/// ```ignore
/// #[derive_impl(frame_system::config_preludes::TestDefaultConfig as frame_system::pallet::DefaultConfig)]
/// impl frame_system::Config for Test {
///     // These are all defined by system as mandatory.
///     type BaseCallFilter = frame_support::traits::Everything;
///     type RuntimeEvent = RuntimeEvent;
///     type RuntimeCall = RuntimeCall;
///     type RuntimeOrigin = RuntimeOrigin;
///     type OnSetCode = ();
///     type PalletInfo = PalletInfo;
///     type Block = Block;
///     // We decide to override this one.
///     type AccountData = pallet_balances::AccountData<u64>;
/// }
/// ```
///
/// where `TestDefaultConfig` was defined and registered as follows:
/// ```ignore
/// pub struct TestDefaultConfig;
///
/// #[register_default_impl(TestDefaultConfig)]
/// impl DefaultConfig for TestDefaultConfig {
///     type Version = ();
///     type BlockWeights = ();
///     type BlockLength = ();
///     type DbWeight = ();
///     type Nonce = u64;
///     type BlockNumber = u64;
///     type Hash = sp_core::hash::H256;
///     type Hashing = sp_runtime::traits::BlakeTwo256;
///     type AccountId = AccountId;
///     type Lookup = IdentityLookup<AccountId>;
///     type BlockHashCount = frame_support::traits::ConstU64<10>;
///     type AccountData = u32;
///     type OnNewAccount = ();
///     type OnKilledAccount = ();
///     type SystemWeightInfo = ();
///     type SS58Prefix = ();
///     type MaxConsumers = frame_support::traits::ConstU32<16>;
/// }
/// ```
///
/// The above call to `derive_impl` would expand to roughly the following:
/// ```ignore
/// impl frame_system::Config for Test {
///     use frame_system::config_preludes::TestDefaultConfig;
///     use frame_system::pallet::DefaultConfig;
///
///     type BaseCallFilter = frame_support::traits::Everything;
///     type RuntimeEvent = RuntimeEvent;
///     type RuntimeCall = RuntimeCall;
///     type RuntimeOrigin = RuntimeOrigin;
///     type OnSetCode = ();
///     type PalletInfo = PalletInfo;
///     type Block = Block;
///     type AccountData = pallet_balances::AccountData<u64>;
///     type Version = <TestDefaultConfig as DefaultConfig>::Version;
///     type BlockWeights = <TestDefaultConfig as DefaultConfig>::BlockWeights;
///     type BlockLength = <TestDefaultConfig as DefaultConfig>::BlockLength;
///     type DbWeight = <TestDefaultConfig as DefaultConfig>::DbWeight;
///     type Nonce = <TestDefaultConfig as DefaultConfig>::Nonce;
///     type BlockNumber = <TestDefaultConfig as DefaultConfig>::BlockNumber;
///     type Hash = <TestDefaultConfig as DefaultConfig>::Hash;
///     type Hashing = <TestDefaultConfig as DefaultConfig>::Hashing;
///     type AccountId = <TestDefaultConfig as DefaultConfig>::AccountId;
///     type Lookup = <TestDefaultConfig as DefaultConfig>::Lookup;
///     type BlockHashCount = <TestDefaultConfig as DefaultConfig>::BlockHashCount;
///     type OnNewAccount = <TestDefaultConfig as DefaultConfig>::OnNewAccount;
///     type OnKilledAccount = <TestDefaultConfig as DefaultConfig>::OnKilledAccount;
///     type SystemWeightInfo = <TestDefaultConfig as DefaultConfig>::SystemWeightInfo;
///     type SS58Prefix = <TestDefaultConfig as DefaultConfig>::SS58Prefix;
///     type MaxConsumers = <TestDefaultConfig as DefaultConfig>::MaxConsumers;
/// }
/// ```
///
/// You can then use the resulting `Test` config in test scenarios.
///
/// Note that items that are _not_ present in our local `DefaultConfig` are automatically copied
/// from the foreign trait (in this case `TestDefaultConfig`) into the local trait impl (in this
/// case `Test`), unless the trait item in the local trait impl is marked with
/// [`#[pallet::no_default]`](`macro@no_default`), in which case it cannot be overridden, and any
/// attempts to do so will result in a compiler error.
///
/// See `frame/examples/default-config/tests.rs` for a runnable end-to-end example pallet that makes
/// use of `derive_impl` to derive its testing config.
///
/// See [here](`macro@config`) for more information and caveats about the auto-generated
/// `DefaultConfig` trait.
///
/// ## Optional Conventions
///
/// Note that as an optional convention, we encourage creating a `config_preludes` module inside of
/// your pallet. This is the convention we follow for `frame_system`'s `TestDefaultConfig` which, as
/// shown above, is located at `frame_system::config_preludes::TestDefaultConfig`. This is just a
/// suggested convention -- there is nothing in the code that expects modules with these names to be
/// in place, so there is no imperative to follow this pattern unless desired.
///
/// In `config_preludes`, you can place types named like:
///
/// * `TestDefaultConfig`
/// * `ParachainDefaultConfig`
/// * `SolochainDefaultConfig`
///
/// Signifying in which context they can be used.
///
/// # Advanced Usage
///
/// ## Expansion
///
/// The `#[derive_impl(default_impl_path as disambiguation_path)]` attribute will expand to the
/// local impl, with any extra items from the foreign impl that aren't present in the local impl
/// also included. In the case of a colliding trait item, the version of the item that exists in the
/// local impl will be retained. All imported items are qualified by the `disambiguation_path`, as
/// discussed above.
///
/// ## Handling of Unnamed Trait Items
///
/// Items that lack a `syn::Ident` for whatever reason are first checked to see if they exist,
/// verbatim, in the local/destination trait before they are copied over, so you should not need to
/// worry about collisions between identical unnamed items.
#[import_tokens_attr_verbatim {
    format!(
        "{}::macro_magic",
        match generate_access_from_frame_or_crate("frame-support") {
            Ok(path) => Ok(path),
            Err(_) => generate_access_from_frame_or_crate("polkadot-sdk-frame"),
        }
        .expect("Failed to find either `frame-support` or `polkadot-sdk-frame` in `Cargo.toml` dependencies.")
        .to_token_stream()
        .to_string()
    )
}]
#[with_custom_parsing(derive_impl::DeriveImplAttrArgs)]
#[proc_macro_attribute]
pub fn derive_impl(attrs: TokenStream, input: TokenStream) -> TokenStream {
	let custom_attrs = parse_macro_input!(__custom_tokens as derive_impl::DeriveImplAttrArgs);
	derive_impl::derive_impl(
		__source_path.into(),
		attrs.into(),
		input.into(),
		custom_attrs.disambiguation_path,
		custom_attrs.no_aggregated_types,
		custom_attrs.generics,
	)
	.unwrap_or_else(|r| r.into_compile_error())
	.into()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::no_default`.
#[proc_macro_attribute]
pub fn no_default(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::no_default_bounds`.
#[proc_macro_attribute]
pub fn no_default_bounds(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Attach this attribute to an impl statement that you want to use with
/// [`#[derive_impl(..)]`](`macro@derive_impl`).
///
/// You must also provide an identifier/name as the attribute's argument. This is the name you
/// must provide to [`#[derive_impl(..)]`](`macro@derive_impl`) when you import this impl via
/// the `default_impl_path` argument. This name should be unique at the crate-level.
///
/// ## Example
///
/// ```ignore
/// pub struct ExampleTestDefaultConfig;
///
/// #[register_default_impl(ExampleTestDefaultConfig)]
/// impl DefaultConfig for ExampleTestDefaultConfig {
/// 	type Version = ();
/// 	type BlockWeights = ();
/// 	type BlockLength = ();
/// 	...
/// 	type SS58Prefix = ();
/// 	type MaxConsumers = frame_support::traits::ConstU32<16>;
/// }
/// ```
///
/// ## Advanced Usage
///
/// This macro acts as a thin wrapper around macro_magic's `#[export_tokens]`. See the docs
/// [here](https://docs.rs/macro_magic/latest/macro_magic/attr.export_tokens.html) for more
/// info.
///
/// There are some caveats when applying a `use` statement to bring a
/// `#[register_default_impl]` item into scope. If you have a `#[register_default_impl]`
/// defined in `my_crate::submodule::MyItem`, it is currently not sufficient to do something
/// like:
///
/// ```ignore
/// use my_crate::submodule::MyItem;
/// #[derive_impl(MyItem as Whatever)]
/// ```
///
/// This will fail with a mysterious message about `__export_tokens_tt_my_item` not being
/// defined.
///
/// You can, however, do any of the following:
/// ```ignore
/// // partial path works
/// use my_crate::submodule;
/// #[derive_impl(submodule::MyItem as Whatever)]
/// ```
/// ```ignore
/// // full path works
/// #[derive_impl(my_crate::submodule::MyItem as Whatever)]
/// ```
/// ```ignore
/// // wild-cards work
/// use my_crate::submodule::*;
/// #[derive_impl(MyItem as Whatever)]
/// ```
#[proc_macro_attribute]
pub fn register_default_impl(attrs: TokenStream, tokens: TokenStream) -> TokenStream {
	// ensure this is a impl statement
	let item_impl = syn::parse_macro_input!(tokens as ItemImpl);

	// internally wrap macro_magic's `#[export_tokens]` macro
	match macro_magic::mm_core::export_tokens_internal(
		attrs,
		item_impl.to_token_stream(),
		true,
		false,
	) {
		Ok(tokens) => tokens.into(),
		Err(err) => err.to_compile_error().into(),
	}
}

/// The optional attribute `#[inject_runtime_type]` can be attached to `RuntimeCall`,
/// `RuntimeEvent`, `RuntimeOrigin` or `PalletInfo` in an impl statement that has
/// `#[register_default_impl]` attached to indicate that this item is generated by
/// `construct_runtime`.
///
/// Attaching this attribute to such an item ensures that the combined impl generated via
/// [`#[derive_impl(..)]`](macro@derive_impl) will use the correct type auto-generated by
/// `construct_runtime!`.
#[doc = docify::embed!("examples/proc_main/inject_runtime_type.rs", derive_impl_works_with_runtime_type_injection)]
/// However, if `no_aggregated_types` is specified while using
/// [`#[derive_impl(..)]`](macro@derive_impl), then these items are attached verbatim to the
/// combined impl.
#[doc = docify::embed!("examples/proc_main/inject_runtime_type.rs", derive_impl_works_with_no_aggregated_types)]
#[proc_macro_attribute]
pub fn inject_runtime_type(_: TokenStream, tokens: TokenStream) -> TokenStream {
	let item = tokens.clone();
	let item = syn::parse_macro_input!(item as TraitItemType);
	if item.ident != "RuntimeCall" &&
		item.ident != "RuntimeEvent" &&
		item.ident != "RuntimeTask" &&
		item.ident != "RuntimeViewFunction" &&
		item.ident != "RuntimeOrigin" &&
		item.ident != "RuntimeHoldReason" &&
		item.ident != "RuntimeFreezeReason" &&
		item.ident != "RuntimeParameters" &&
		item.ident != "PalletInfo"
	{
		return syn::Error::new_spanned(
			item,
			"`#[inject_runtime_type]` can only be attached to `RuntimeCall`, `RuntimeEvent`, \
			`RuntimeTask`, `RuntimeViewFunction`, `RuntimeOrigin`, `RuntimeParameters` or `PalletInfo`",
		)
		.to_compile_error()
		.into();
	}
	tokens
}

/// Used internally to decorate pallet attribute macro stubs when they are erroneously used
/// outside of a pallet module
fn pallet_macro_stub() -> TokenStream {
	quote!(compile_error!(
		"This attribute can only be used from within a pallet module marked with `#[frame_support::pallet]`"
	))
	.into()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::config`.
#[proc_macro_attribute]
pub fn config(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::constant`.
#[proc_macro_attribute]
pub fn constant(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::constant_name`.
#[proc_macro_attribute]
pub fn constant_name(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at
/// `frame_support::pallet_macros::disable_frame_system_supertrait_check`.
#[proc_macro_attribute]
pub fn disable_frame_system_supertrait_check(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::storage_version`.
#[proc_macro_attribute]
pub fn storage_version(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::hooks`.
#[proc_macro_attribute]
pub fn hooks(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::weight`.
#[proc_macro_attribute]
pub fn weight(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::compact`.
#[proc_macro_attribute]
pub fn compact(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::call`.
#[proc_macro_attribute]
pub fn call(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Each dispatchable may also be annotated with the `#[pallet::call_index($idx)]` attribute,
/// which explicitly defines the codec index for the dispatchable function in the `Call` enum.
///
/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::call_index`.
#[proc_macro_attribute]
pub fn call_index(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::feeless_if`.
#[proc_macro_attribute]
pub fn feeless_if(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::extra_constants`.
#[proc_macro_attribute]
pub fn extra_constants(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::error`.
#[proc_macro_attribute]
pub fn error(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::event`.
#[proc_macro_attribute]
pub fn event(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::include_metadata`.
#[proc_macro_attribute]
pub fn include_metadata(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::generate_deposit`.
#[proc_macro_attribute]
pub fn generate_deposit(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::storage`.
#[proc_macro_attribute]
pub fn storage(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::getter`.
#[proc_macro_attribute]
pub fn getter(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::storage_prefix`.
#[proc_macro_attribute]
pub fn storage_prefix(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::unbounded`.
#[proc_macro_attribute]
pub fn unbounded(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::whitelist_storage`.
#[proc_macro_attribute]
pub fn whitelist_storage(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at
/// `frame_support::pallet_macros::disable_try_decode_storage`.
#[proc_macro_attribute]
pub fn disable_try_decode_storage(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::type_value`.
#[proc_macro_attribute]
pub fn type_value(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::genesis_config`.
#[proc_macro_attribute]
pub fn genesis_config(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::genesis_build`.
#[proc_macro_attribute]
pub fn genesis_build(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::inherent`.
#[proc_macro_attribute]
pub fn inherent(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::validate_unsigned`.
#[proc_macro_attribute]
pub fn validate_unsigned(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at
/// `frame_support::pallet_macros::view_functions`.
#[proc_macro_attribute]
pub fn view_functions(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::origin`.
#[proc_macro_attribute]
pub fn origin(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// Documentation for this macro can be found at `frame_support::pallet_macros::composite_enum`.
#[proc_macro_attribute]
pub fn composite_enum(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows you to define some service work that can be recognized by the off-chain worker.
///
/// The off-chain worker can then create and submit all such work items at any given time.
///
/// These work items are defined as instances of the `Task` trait (found at
/// `frame_support::traits::Task`). [`pallet:tasks_experimental`](macro@tasks_experimental) when
/// attached to an `impl` block inside a pallet, will generate an enum `Task<T>` whose variants
/// are mapped to functions inside this `impl` block.
///
/// Each such function must have the following set of attributes:
///
/// * [`pallet::task_list`](macro@task_list)
/// * [`pallet::task_condition`](macro@task_condition)
/// * [`pallet::task_weight`](macro@task_weight)
/// * [`pallet::task_index`](macro@task_index)
///
/// All of such Tasks are then aggregated into a `RuntimeTask` by
/// [`construct_runtime`](macro@construct_runtime).
///
/// Finally, the `RuntimeTask` can then be used by the off-chain worker to create and
/// submit such tasks via an extrinsic defined in `frame_system` called `do_task` which accepts
/// unsigned transaction from local source.
///
/// When submitted as unsigned transactions, note that the tasks will be executed in a random order.
///
/// ## Example
#[doc = docify::embed!("examples/proc_main/tasks.rs", tasks_example)]
/// Now, this can be executed as follows:
#[doc = docify::embed!("examples/proc_main/tasks.rs", tasks_work)]
#[proc_macro_attribute]
pub fn tasks_experimental(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows defining an iterator over available work items for a task.
///
/// This attribute is attached to a function inside an `impl` block annotated with
/// [`pallet::tasks_experimental`](macro@tasks_experimental).
///
/// It takes an iterator as input that yields a tuple with same types as the function
/// arguments.
#[proc_macro_attribute]
pub fn task_list(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows defining conditions for a task to run.
///
/// This attribute is attached to a function inside an `impl` block annotated with
/// [`pallet::tasks_experimental`](macro@tasks_experimental) to define the conditions for a
/// given work item to be valid.
///
/// It takes a closure as input, which is then used to define the condition. The closure
/// should have the same signature as the function it is attached to, except that it should
/// return a `bool` instead.
#[proc_macro_attribute]
pub fn task_condition(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows defining the weight of a task.
///
/// This attribute is attached to a function inside an `impl` block annotated with
/// [`pallet::tasks_experimental`](macro@tasks_experimental) define the weight of a given work
/// item.
///
/// It takes a closure as input, which should return a `Weight` value.
#[proc_macro_attribute]
pub fn task_weight(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows defining an index for a task.
///
/// This attribute is attached to a function inside an `impl` block annotated with
/// [`pallet::tasks_experimental`](macro@tasks_experimental) to define the index of a given
/// work item.
///
/// It takes an integer literal as input, which is then used to define the index. This
/// index should be unique for each function in the `impl` block.
#[proc_macro_attribute]
pub fn task_index(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::pallet_section`.
#[proc_macro_attribute]
pub fn pallet_section(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	let tokens_clone = tokens.clone();
	// ensure this can only be attached to a module
	let _mod = parse_macro_input!(tokens_clone as ItemMod);

	// use macro_magic's export_tokens as the internal implementation otherwise
	match macro_magic::mm_core::export_tokens_internal(attr, tokens, false, true) {
		Ok(tokens) => tokens.into(),
		Err(err) => err.to_compile_error().into(),
	}
}

/// ---
///
/// **Rust-Analyzer users**: See the documentation of the Rust item in
/// `frame_support::pallet_macros::import_section`.
#[import_tokens_attr {
    format!(
        "{}::macro_magic",
        match generate_access_from_frame_or_crate("frame-support") {
            Ok(path) => Ok(path),
            Err(_) => generate_access_from_frame_or_crate("polkadot-sdk-frame"),
        }
        .expect("Failed to find either `frame-support` or `polkadot-sdk-frame` in `Cargo.toml` dependencies.")
        .to_token_stream()
        .to_string()
    )
}]
#[proc_macro_attribute]
pub fn import_section(attr: TokenStream, tokens: TokenStream) -> TokenStream {
	let foreign_mod = parse_macro_input!(attr as ItemMod);
	let mut internal_mod = parse_macro_input!(tokens as ItemMod);

	// check that internal_mod is a pallet module
	if !internal_mod.attrs.iter().any(|attr| {
		if let Some(last_seg) = attr.path().segments.last() {
			last_seg.ident == "pallet"
		} else {
			false
		}
	}) {
		return Error::new(
			internal_mod.ident.span(),
			"`#[import_section]` can only be applied to a valid pallet module",
		)
		.to_compile_error()
		.into();
	}

	if let Some(ref mut content) = internal_mod.content {
		if let Some(foreign_content) = foreign_mod.content {
			content.1.extend(foreign_content.1);
		}
	}

	quote! {
		#internal_mod
	}
	.into()
}

/// Construct a runtime, with the given name and the given pallets.
///
/// # Example:
#[doc = docify::embed!("examples/proc_main/runtime.rs", runtime_macro)]
/// # Supported Attributes:
///
/// ## Legacy Ordering
///
/// An optional attribute can be defined as #[frame_support::runtime(legacy_ordering)] to
/// ensure that the order of hooks is same as the order of pallets (and not based on the
/// pallet_index). This is to support legacy runtimes and should be avoided for new ones.
///
/// # Note
///
/// The population of the genesis storage depends on the order of pallets. So, if one of your
/// pallets depends on another pallet, the pallet that is depended upon needs to come before
/// the pallet depending on it.
///
/// # Type definitions
///
/// * The macro generates a type alias for each pallet to their `Pallet`. E.g. `type System =
///   frame_system::Pallet<Runtime>`
#[proc_macro_attribute]
pub fn runtime(attr: TokenStream, item: TokenStream) -> TokenStream {
	runtime::runtime(attr, item)
}

/// Mark a module that contains dynamic parameters.
///
/// See the `pallet_parameters` for a full example.
///
/// # Arguments
///
/// The macro accepts two positional arguments, of which the second is optional.
///
/// ## Aggregated Enum Name
///
/// This sets the name that the aggregated Key-Value enum will be named after. Common names would be
/// `RuntimeParameters`, akin to `RuntimeCall`, `RuntimeOrigin` etc. There is no default value for
/// this argument.
///
/// ## Parameter Storage Backend
///
/// The second argument provides access to the storage of the parameters. It can either be set on
/// on this attribute, or on the inner ones. If set on both, the inner one takes precedence.
#[proc_macro_attribute]
pub fn dynamic_params(attrs: TokenStream, input: TokenStream) -> TokenStream {
	dynamic_params::dynamic_params(attrs.into(), input.into())
		.unwrap_or_else(|r| r.into_compile_error())
		.into()
}

/// Define a module inside a [`macro@dynamic_params`] module that contains dynamic parameters.
///
/// See the `pallet_parameters` for a full example.
///
/// # Argument
///
/// This attribute takes one optional argument. The argument can either be put here or on the
/// surrounding `#[dynamic_params]` attribute. If set on both, the inner one takes precedence.
#[proc_macro_attribute]
pub fn dynamic_pallet_params(attrs: TokenStream, input: TokenStream) -> TokenStream {
	dynamic_params::dynamic_pallet_params(attrs.into(), input.into())
		.unwrap_or_else(|r| r.into_compile_error())
		.into()
}

/// Used internally by [`dynamic_params`].
#[doc(hidden)]
#[proc_macro_attribute]
pub fn dynamic_aggregated_params_internal(attrs: TokenStream, input: TokenStream) -> TokenStream {
	dynamic_params::dynamic_aggregated_params_internal(attrs.into(), input.into())
		.unwrap_or_else(|r| r.into_compile_error())
		.into()
}

/// Allows to authorize some general transactions with specific dispatchable functions
/// (dispatchable functions a.k.a. calls).
///
/// This attribute allows to specify a special validation logic for a specific call.
/// A general transaction with this specific call can then be validated by the given function,
/// and if valid then dispatched with the origin `frame_system::Origin::Authorized`.
///
/// To ensure the origin of the call is the authorization process, the call must check the origin
/// with `frame_system::ensure_authorized` function.
///
/// To enable the authorization process on the extrinsic, the runtime must use
/// `frame_system::AuthorizeCall` transaction extension in the transaction extension pipeline.
///
/// To enable the creation of authorized call from offchain worker. The runtime should implement
/// `frame_system::CreateAuthorizedTransaction`. This trait allows to specify which transaction
/// extension to use when creating a transaction for an authorized call.
///
/// # Usage in the pallet
///
/// ## Example/Overview:
///
/// ```
/// # #[allow(unused)]
/// #[frame_support::pallet]
/// pub mod pallet {
///     use frame_support::pallet_prelude::*;
///     use frame_system::pallet_prelude::*;
///
///     #[pallet::pallet]
///     pub struct Pallet<T>(_);
///
///     #[pallet::config]
///     pub trait Config: frame_system::Config {}
///
///     #[pallet::call]
///     impl<T: Config> Pallet<T> {
///         #[pallet::weight(Weight::zero())]
///         #[pallet::authorize(|_source, foo| if *foo == 42 {
///             // The amount to refund, here we refund nothing
///             let refund = Weight::zero();
///             // The validity, here we accept the call and it provides itself.
///             // See `ValidTransaction` for more information.
///             let validity = ValidTransaction::with_tag_prefix("my-pallet")
///                 .and_provides("some_call")
///                 .into();
///             Ok((validity, refund))
///         } else {
///             Err(TransactionValidityError::Invalid(InvalidTransaction::Call))
///         })]
///         #[pallet::weight_of_authorize(Weight::zero())]
///         #[pallet::call_index(0)]
///         pub fn some_call(origin: OriginFor<T>, arg: u32) -> DispatchResult {
///             ensure_authorized(origin)?;
///
///             Ok(())
///         }
///
///         #[pallet::weight(Weight::zero())]
///         // We can also give the callback as a function
///         #[pallet::authorize(Self::authorize_some_other_call)]
///         #[pallet::weight_of_authorize(Weight::zero())]
///         #[pallet::call_index(1)]
///         pub fn some_other_call(origin: OriginFor<T>, arg: u32) -> DispatchResult {
///             ensure_authorized(origin)?;
///
///             Ok(())
///         }
///     }
///
///     impl<T: Config> Pallet<T> {
///         fn authorize_some_other_call(
///             source: TransactionSource,
///             foo: &u32
///         ) -> TransactionValidityWithRefund {
///             if *foo == 42 {
///                 let refund = Weight::zero();
///                 let validity = ValidTransaction::default();
///                 Ok((validity, refund))
///             } else {
///                 Err(TransactionValidityError::Invalid(InvalidTransaction::Call))
///             }
///         }
///     }
///
///     #[frame_benchmarking::v2::benchmarks]
///     mod benchmarks {
///         use super::*;
///         use frame_benchmarking::v2::BenchmarkError;
///
///         #[benchmark]
///         fn authorize_some_call() -> Result<(), BenchmarkError> {
///             let call = Call::<T>::some_call { arg: 42 };
///
///             #[block]
///             {
///                 use frame_support::pallet_prelude::Authorize;
///                 call.authorize(TransactionSource::External)
///                     .ok_or("Call must give some authorization")??;
///             }
///
///             Ok(())
///         }
///     }
/// }
/// ```
///
/// ## Specification:
///
/// Authorize process comes with 2 attributes macro on top of the authorized call:
///
/// * `#[pallet::authorize($authorized_function)]` - defines the function that authorizes the call.
///   First argument is the transaction source `TransactionSource` then followed by the same as call
///   arguments but by reference `&`. Return type is `TransactionValidityWithRefund`.
/// * `#[pallet::weight_of_authorize($weight)]` - defines the value of the weight of the authorize
///   function. This attribute is similar to `#[pallet::weight]`:
///   * it can be ignore in `dev_mode`
///   * it can be automatically infered from weight info. For the call `foo` the function
///     `authorize_foo` in the weight info will be used. (weight info needs to be provided in the
///     call attribute: `#[pallet::call(weight = T::WeightInfo)]`).
///   * it can be a fixed value like `Weight::from_all(0)` (not recommended in production).
///
///   The weight must be small enough so that nodes don't get DDOS by validating transactions.
///
/// Then in the call it must be ensured that the origin is the authorization process. This can
/// be done using `frame_system::ensure_authorized` function.
///
/// # The macro expansion
///
/// From the given "authorize" function and weight, the macro will implement the trait
/// `Authorize` on the call.
///
/// # How to benchmark
///
/// The authorize function is used as the implementation of the trait
/// `Authorize` for the call.
/// To benchmark a call variant, use the function
/// `Authorize::authorize` on a call value.
/// See the example in the first section.
#[proc_macro_attribute]
pub fn authorize(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}

/// Allows to define the weight of the authorize function.
///
/// See [`authorize`](macro@authorize) for more information on how authorization works.
///
/// Defines the value of the weight of the authorize function. This attribute is similar to
/// `#[pallet::weight]`:
/// * it can be ignore in `dev_mode`
/// * it can be automatically infered from weight info. For the call `foo` the function
///   `authorize_foo` in the weight info will be used.
/// * it can be a fixed value like `Weight::from_all(0)` (not recommended in production).
#[proc_macro_attribute]
pub fn weight_of_authorize(_: TokenStream, _: TokenStream) -> TokenStream {
	pallet_macro_stub()
}
