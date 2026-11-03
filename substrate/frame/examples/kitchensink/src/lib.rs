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

//! # Kitchensink Example Pallet
//!
//! **This pallet serves as an example and is not meant to be used in production.**
//!
//! The kitchen-sink catalog of the the FRAME macros and their various syntax options.
//!
//! This example does not focus on pallet instancing, `dev_mode`, and does nto include any 'where'
//! clauses on `T`. These will both incur additional complexity to the syntax, but are not discussed
//! here.

#![cfg_attr(not(feature = "std"), no_std)]

// Re-export pallet items so that they can be accessed from the crate namespace.
pub use pallet::*;

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;

#[cfg(feature = "try-runtime")]
use sp_runtime::TryRuntimeError;

pub mod weights;
pub use weights::*;

extern crate alloc;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	/// The config trait of the pallet. You can basically do anything with the config trait that you
	/// can do with a normal rust trait: import items consisting of types, constants and functions.
	///
	/// A very common pattern is for a pallet to import implementations of traits such as
	/// [`frame_support::traits::Currency`], [`frame_support::traits::fungibles::Inspect`] and
	/// [`frame_support::traits::Get`]. These are all types that the pallet is delegating to the top
	/// level runtime to provide to it.
	///
	/// The `FRAME`-specific syntax are:
	///
	/// * the use of `#[pallet::constant]`([`frame_support::procedural`]), which places a `Get`
	///   implementation in the metadata.
	/// * `type RuntimeEvent`, which is mandatory if your pallet has events. See [`Event`].
	/// * Needless to say, because [`Config`] is bounded by [`frame_system::Config`], you can use
	///   all the items from [`frame_system::Config`] as well, such as `AccountId`.
	/// * `#[pallet::disable_frame_system_supertrait_check]` would remove the need for
	///   `frame_system::Config` to exist, which you should almost never need.
	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Type representing the weight of this pallet
		type WeightInfo: WeightInfo;

		/// This is a normal Rust type, nothing specific to FRAME here.
		type Currency: frame_support::traits::fungible::Inspect<Self::AccountId>;

		/// Similarly, let the runtime decide this.
		fn some_function() -> u32;

		/// And this
		const FOO: u32;

		/// This is a FRAME-specific item. It will be placed in the metadata of the pallet, and
		/// therefore can be queried by offchain applications.
		#[pallet::constant]
		type InMetadata: Get<u32>;
	}

	/// Allows you to define some extra constants to be added into constant metadata.
	#[pallet::extra_constants]
	impl<T: Config> Pallet<T> {
		#[allow(non_snake_case)]
		fn SomeValue() -> u32 {
			unimplemented!()
		}

		#[pallet::constant_name(OtherValue)]
		fn arbitrary_name() -> u32 {
			unimplemented!()
		}
	}

	const STORAGE_VERSION: frame_support::traits::StorageVersion = StorageVersion::new(1);

	/// The pallet struct. There's nothing special to FRAME about this; it can implement functions
	/// in an impl blocks, traits and so on.
	#[pallet::pallet]
	#[pallet::without_storage_info]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	/// Allows you to define some origin for the pallet.
	#[pallet::origin]
	pub type Origin<T> = frame_system::RawOrigin<<T as frame_system::Config>::AccountId>;

	// first, we showcase all the possible storage types, with most of their details.

	/// A storage value. We mark this as unbounded, alter its prefix, and define a custom storage
	/// getter for it.
	///
	/// The value is stored a single trie node, and therefore can be retrieved with a single
	/// database access.
	#[pallet::storage]
	#[pallet::unbounded] // optional
	#[pallet::storage_prefix = "OtherFoo"] // optional
	pub type Foo<T> = StorageValue<Value = u32>;

	#[pallet::type_value]
	pub fn DefaultForFoo() -> u32 {
		1
	}

	#[pallet::storage]
	pub type FooWithDefault<T> =
		StorageValue<Value = u32, QueryKind = ValueQuery, OnEmpty = DefaultForFoo>;

	/// A storage map. This creates a mapping from keys of type `u32` to values of type `u32`.
	///
	/// Keys and values can be iterated, albeit each value is stored under a unique trie key,
	/// meaning that an iteration consists of many database accesses.
	#[pallet::storage]
	pub type Bar<T> = StorageMap<Hasher = Blake2_128Concat, Key = u32, Value = u32>;

	/// Conceptually same as `StorageMap<>` where the key is a tuple of `(u32, u32)`. On top, it
	/// provides some functions to iterate or remove items based on only the first key.
	#[pallet::storage]
	pub type Qux<T> = StorageDoubleMap<
		Hasher1 = Blake2_128Concat,
		Key1 = u32,
		Hasher2 = Blake2_128Concat,
		Key2 = u32,
		Value = u32,
	>;

	/// Same as `StorageDoubleMap`, but with arbitrary number of keys.
	#[pallet::storage]
	pub type Quux<T> = StorageNMap<
		Key = (
			NMapKey<Blake2_128Concat, u8>,
			NMapKey<Blake2_128Concat, u16>,
			NMapKey<Blake2_128Concat, u32>,
		),
		Value = u64,
	>;

	/// In all of these examples, we chose a syntax where the storage item is defined using the
	/// explicit generic syntax (`X = Y`). Alternatively:
	#[pallet::storage]
	pub type AlternativeSyntax<T> = StorageMap<_, Blake2_128Concat, u32, u32>;

	/// Lastly, all storage items, as you saw, had to be generic over `T`. If they want to use an
	/// item from `Config`, `<T: Config>` should be used.
	#[pallet::storage]
	pub type AlternativeSyntax2<T: Config> = StorageMap<_, Blake2_128Concat, T::AccountId, u32>;

	/// The genesis config type. This allows the pallet to define how it should initialized upon
	/// genesis.
	///
	/// It can be generic over `T` or not, depending on whether it is or not.
	#[pallet::genesis_config]
	pub struct GenesisConfig<T: Config> {
		pub foo: u32,
		pub bar: BlockNumberFor<T>,
	}

	impl<T: Config> Default for GenesisConfig<T> {
		fn default() -> Self {
			Self { foo: 0, bar: Default::default() }
		}
	}

	/// Allows you to define how `genesis_configuration is built.
	#[pallet::genesis_build]
	impl<T: Config> BuildGenesisConfig for GenesisConfig<T> {
		fn build(&self) {
			Foo::<T>::put(self.foo);
		}
	}

	/// The call declaration. This states the entry points that we handle. The
	/// macro takes care of the marshalling of arguments and dispatch.
	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::set_foo_benchmark())]
		/// Marks this call as feeless if `new_foo` is zero.
		#[pallet::feeless_if(|_origin: &OriginFor<T>, new_foo: &u32, _other_compact: &u128| -> bool {
			*new_foo == 0
		})]
		pub fn set_foo(
			_: OriginFor<T>,
			new_foo: u32,
			#[pallet::compact] _other_compact: u128,
		) -> DispatchResult {
			Foo::<T>::set(Some(new_foo));

			Ok(())
		}

		/// A call that is specially authorized.
		/// Authorized call can be dispatched by anybody without requiring any signature or fee.
		#[pallet::call_index(1)]
		#[pallet::authorize(|
			_source: TransactionSource,
			new_foo: &u32,
		| -> TransactionValidityWithRefund {
			if *new_foo == 42 && Foo::<T>::get().is_none() {
				// This is the amount to refund, here we refund nothing.
				let refund = Weight::zero();
				// The transaction needs to give a provided tag.
				// See `ValidTransaction` documentation.
				let validity = ValidTransaction::with_tag_prefix("pallet-kitchen-sink")
					.and_provides("set_foo_using_authorize")
					.into();
				Ok((validity, refund))
			} else {
				Err(InvalidTransaction::Call.into())
			}
		})]
		#[pallet::weight(T::WeightInfo::set_foo_using_authorize())]
		#[pallet::weight_of_authorize(T::WeightInfo::authorize_set_foo_using_authorize())]
		pub fn set_foo_using_authorize(origin: OriginFor<T>, new_foo: u32) -> DispatchResult {
			// We only dispatch if it comes from the authorized origin. Meaning that the closure
			// passed in `pallet::authorize` has successfully authorized the call.
			ensure_authorized(origin)?;
			Foo::<T>::set(Some(new_foo));

			Ok(())
		}
	}

	/// The event type. This exactly like a normal Rust enum.
	///
	/// It can or cannot be generic over `<T: Config>`. Note that unlike a normal enum, if none of
	/// the variants actually use `<T: Config>`, the macro will generate a hidden `PhantomData`
	/// variant.
	///
	/// The `generate_deposit` macro generates a function on `Pallet` called `deposit_event` which
	/// will properly convert the error type of your pallet into `RuntimeEvent` (recall `type
	/// RuntimeEvent: From<Event<Self>>`, so it can be converted) and deposit it via
	/// `frame_system::Pallet::deposit_event`.
	#[pallet::event]
	#[pallet::generate_deposit(pub fn deposit_event)]
	pub enum Event<T: Config> {
		/// A simple tuple style variant.
		SomethingHappened(u32),
		/// A simple struct-style variant. Note that we use `AccountId` from `T` because `T:
		/// Config`, which by extension implies `T: frame_system::Config`.
		SomethingDetailedHappened { at: u32, to: T::AccountId },
		/// Another variant.
		SomeoneJoined(T::AccountId),
	}

	/// The error enum. Must always be generic over `<T>`, which is expanded to `<T: Config>`.
	#[pallet::error]
	pub enum Error<T> {
		SomethingWentWrong,
		SomethingBroke,
	}

	/// All the possible hooks that a pallet can have. See [`frame_support::traits::Hooks`] for more
	/// info.
	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {}

		fn offchain_worker(_n: BlockNumberFor<T>) {
			unimplemented!()
		}

		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			unimplemented!()
		}

		fn on_finalize(_n: BlockNumberFor<T>) {
			unimplemented!()
		}

		fn on_idle(_n: BlockNumberFor<T>, _remaining_weight: Weight) -> Weight {
			unimplemented!()
		}

		fn on_runtime_upgrade() -> Weight {
			unimplemented!()
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, TryRuntimeError> {
			unimplemented!()
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(_state: Vec<u8>) -> Result<(), TryRuntimeError> {
			unimplemented!()
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), TryRuntimeError> {
			unimplemented!()
		}
	}

	/// Allows you to define an enum on the pallet which will then instruct `construct_runtime` to
	/// amalgamate all similarly-named enums from other pallets into an aggregate enum.
	#[pallet::composite_enum]
	pub enum HoldReason {
		Staking,
	}

	/// Allows the pallet to provide some inherent. See [`frame_support::inherent::ProvideInherent`]
	/// for more info.
	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = MakeFatalError<()>;

		const INHERENT_IDENTIFIER: [u8; 8] = *b"test1234";

		fn create_inherent(_data: &InherentData) -> Option<Self::Call> {
			unimplemented!();
		}

		fn is_inherent(_call: &Self::Call) -> bool {
			unimplemented!()
		}
	}
}
