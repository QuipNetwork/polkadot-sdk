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

//! Traits for encoding data related to pallet's storage items.

use alloc::{collections::btree_set::BTreeSet, vec, vec::Vec};
use codec::{Decode, DecodeWithMemTracking, Encode, FullCodec, MaxEncodedLen};
use core::{marker::PhantomData, mem, ops::Drop};
use frame_support::CloneNoBound;
use impl_trait_for_tuples::impl_for_tuples;
use scale_info::TypeInfo;
pub use sp_core::storage::TrackedStorageKey;
use sp_core::Get;
use sp_runtime::{
	traits::{Convert, Member},
	Debug, DispatchError,
};

/// An instance of a pallet in the storage.
///
/// It is required that these instances are unique, to support multiple instances per pallet in the
/// same runtime!
///
/// E.g. for module MyModule default instance will have prefix "MyModule" and other instances
/// "InstanceNMyModule".
pub trait Instance: 'static {
	/// Unique module prefix. E.g. "InstanceNMyModule" or "MyModule"
	const PREFIX: &'static str;
	/// Unique numerical identifier for an instance.
	const INDEX: u8;
}

// Dummy implementation for `()`.
impl Instance for () {
	const PREFIX: &'static str = "";
	const INDEX: u8 = 0;
}

/// An instance of a storage in a pallet.
///
/// Define an instance for an individual storage inside a pallet.
/// The pallet prefix is used to isolate the storage between pallets, and the storage prefix is
/// used to isolate storages inside a pallet.
///
/// NOTE: These information can be used to define storages in pallet such as a `StorageMap` which
/// can use keys after `twox_128(pallet_prefix())++twox_128(STORAGE_PREFIX)`
pub trait StorageInstance {
	/// Prefix of a pallet to isolate it from other pallets.
	fn pallet_prefix() -> &'static str;

	/// Return the prefix hash of pallet instance.
	///
	/// NOTE: This hash must be `twox_128(pallet_prefix())`.
	/// Should not impl this function by hand. Only use the default or macro generated impls.
	fn pallet_prefix_hash() -> [u8; 16] {
		sp_io::hashing::twox_128(Self::pallet_prefix().as_bytes())
	}

	/// Prefix given to a storage to isolate from other storages in the pallet.
	const STORAGE_PREFIX: &'static str;

	/// Return the prefix hash of storage instance.
	///
	/// NOTE: This hash must be `twox_128(STORAGE_PREFIX)`.
	fn storage_prefix_hash() -> [u8; 16] {
		sp_io::hashing::twox_128(Self::STORAGE_PREFIX.as_bytes())
	}

	/// Return the prefix hash of instance.
	///
	/// NOTE: This hash must be `twox_128(pallet_prefix())++twox_128(STORAGE_PREFIX)`.
	/// Should not impl this function by hand. Only use the default or macro generated impls.
	fn prefix_hash() -> [u8; 32] {
		let mut final_key = [0u8; 32];
		final_key[..16].copy_from_slice(&Self::pallet_prefix_hash());
		final_key[16..].copy_from_slice(&Self::storage_prefix_hash());

		final_key
	}
}

/// Metadata about storage from the runtime.
#[derive(Debug, codec::Encode, codec::Decode, Eq, PartialEq, Clone, scale_info::TypeInfo)]
pub struct StorageInfo {
	/// Encoded string of pallet name.
	pub pallet_name: Vec<u8>,
	/// Encoded string of storage name.
	pub storage_name: Vec<u8>,
	/// The prefix of the storage. All keys after the prefix are considered part of this storage.
	pub prefix: Vec<u8>,
	/// The maximum number of values in the storage, or none if no maximum specified.
	pub max_values: Option<u32>,
	/// The maximum size of key/values in the storage, or none if no maximum specified.
	pub max_size: Option<u32>,
}

/// A trait to give information about storage.
///
/// It can be used to calculate PoV worst case size.
pub trait StorageInfoTrait {
	fn storage_info() -> Vec<StorageInfo>;
}

#[cfg_attr(all(not(feature = "tuples-96"), not(feature = "tuples-128")), impl_for_tuples(64))]
#[cfg_attr(all(feature = "tuples-96", not(feature = "tuples-128")), impl_for_tuples(96))]
#[cfg_attr(feature = "tuples-128", impl_for_tuples(128))]
impl StorageInfoTrait for Tuple {
	fn storage_info() -> Vec<StorageInfo> {
		let mut res = vec![];
		for_tuples!( #( res.extend_from_slice(&Tuple::storage_info()); )* );
		res
	}
}

/// Similar to [`StorageInfoTrait`], a trait to give partial information about storage.
///
/// This is useful when a type can give some partial information with its generic parameter doesn't
/// implement some bounds.
pub trait PartialStorageInfoTrait {
	fn partial_storage_info() -> Vec<StorageInfo>;
}

/// Allows a pallet to specify storage keys to whitelist during benchmarking.
/// This means those keys will be excluded from the benchmarking performance
/// calculation.
pub trait WhitelistedStorageKeys {
	/// Returns a [`Vec<TrackedStorageKey>`] indicating the storage keys that
	/// should be whitelisted during benchmarking. This means that those keys
	/// will be excluded from the benchmarking performance calculation.
	fn whitelisted_storage_keys() -> Vec<TrackedStorageKey>;
}

#[cfg_attr(all(not(feature = "tuples-96"), not(feature = "tuples-128")), impl_for_tuples(64))]
#[cfg_attr(all(feature = "tuples-96", not(feature = "tuples-128")), impl_for_tuples(96))]
#[cfg_attr(feature = "tuples-128", impl_for_tuples(128))]
impl WhitelistedStorageKeys for Tuple {
	fn whitelisted_storage_keys() -> Vec<TrackedStorageKey> {
		// de-duplicate the storage keys
		let mut combined_keys: BTreeSet<TrackedStorageKey> = BTreeSet::new();
		for_tuples!( #(
			for storage_key in Tuple::whitelisted_storage_keys() {
				combined_keys.insert(storage_key);
			}
		 )* );
		combined_keys.into_iter().collect::<Vec<_>>()
	}
}

/// The resource footprint of a bunch of blobs. We assume only the number of blobs and their total
/// size in bytes matter.
#[derive(
	Default,
	Copy,
	Clone,
	Eq,
	PartialEq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
	Decode,
	Encode,
	DecodeWithMemTracking,
)]
pub struct Footprint {
	/// The number of blobs.
	pub count: u64,
	/// The total size of the blobs in bytes.
	pub size: u64,
}

impl Footprint {
	/// Construct a footprint directly from `items` and `len`.
	pub fn from_parts(items: usize, len: usize) -> Self {
		Self { count: items as u64, size: len as u64 }
	}

	/// Construct a footprint with one item, and size equal to the encoded size of `e`.
	pub fn from_encodable(e: impl Encode) -> Self {
		Self::from_parts(1, e.encoded_size())
	}

	/// Construct a footprint with one item, and size equal to the max encoded length of `E`.
	pub fn from_mel<E: MaxEncodedLen>() -> Self {
		Self::from_parts(1, E::max_encoded_len())
	}
}

/// A storage price that increases linearly with the number of elements and their size.
pub struct LinearStoragePrice<Base, Slope, Balance>(PhantomData<(Base, Slope, Balance)>);
impl<Base, Slope, Balance> Convert<Footprint, Balance> for LinearStoragePrice<Base, Slope, Balance>
where
	Base: Get<Balance>,
	Slope: Get<Balance>,
	Balance: From<u64> + sp_runtime::Saturating,
{
	fn convert(a: Footprint) -> Balance {
		let s: Balance = (a.count.saturating_mul(a.size)).into();
		s.saturating_mul(Slope::get()).saturating_add(Base::get())
	}
}

/// Constant `Price` regardless of the given [`Footprint`].
pub struct ConstantStoragePrice<Price, Balance>(PhantomData<(Price, Balance)>);
impl<Price, Balance> Convert<Footprint, Balance> for ConstantStoragePrice<Price, Balance>
where
	Price: Get<Balance>,
	Balance: From<u64> + sp_runtime::Saturating,
{
	fn convert(_: Footprint) -> Balance {
		Price::get()
	}
}

/// Placeholder marking functionality disabled. Useful for disabling various (sub)features.
#[derive(CloneNoBound, Debug, Encode, Eq, Decode, TypeInfo, MaxEncodedLen, PartialEq)]
pub struct Disabled;
impl<A, F> Consideration<A, F> for Disabled {
	fn new(_: &A, _: F) -> Result<Self, DispatchError> {
		Err(DispatchError::Other("Disabled"))
	}
	fn update(self, _: &A, _: F) -> Result<Self, DispatchError> {
		Err(DispatchError::Other("Disabled"))
	}
	fn drop(self, _: &A) -> Result<(), DispatchError> {
		Ok(())
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(_: &A, _: F) {}
}

/// Some sort of cost taken from account temporarily in order to offset the cost to the chain of
/// holding some data [`Footprint`] in state.
///
/// The cost may be increased, reduced or dropped entirely as the footprint changes.
///
/// A single ticket corresponding to some particular datum held in storage. This is an opaque
/// type, but must itself be stored and generally it should be placed alongside whatever data
/// the ticket was created for and the attributable account.
///
/// Tickets are generally attributable to a single account for their whole lifetime. Calling
/// `update`, `burn` or `drop` with a different account than `new` was called with may result in
/// wrong accounting since the ticket type is not required to store the attributable account.
///
/// While not technically a linear type owing to the need for `FullCodec`, *this should be
/// treated as one*. Don't type to duplicate it, and remember to drop it when you're done with
/// it.
#[must_use]
pub trait Consideration<AccountId, Footprint>:
	Member + FullCodec + TypeInfo + MaxEncodedLen
{
	/// Create a ticket for the `new` footprint attributable to `who`. This ticket *must* ultimately
	/// be consumed through `update` or `drop` once the footprint changes or is removed.
	fn new(who: &AccountId, new: Footprint) -> Result<Self, DispatchError>;

	/// Consume an old ticket to modify its footprint without altering the attributable account.
	///
	/// For creating tickets and dropping them, you can use the simpler `new` and `drop` instead.
	/// Note that this must be called with the same account as `new` was.
	fn update(self, who: &AccountId, new: Footprint) -> Result<Self, DispatchError>;

	/// Consume a ticket for some `old` footprint attributable to `who` which should now been freed.
	fn drop(self, who: &AccountId) -> Result<(), DispatchError>;

	/// Consume a ticket for some `old` footprint attributable to `who` which should be sacrificed.
	///
	/// This is infallible. In the general case (and it is left unimplemented), then it is
	/// equivalent to the consideration never being dropped. Cases which can handle this properly
	/// should implement, but it *MUST* rely on the loss of the consideration to the owner.
	fn burn(self, _: &AccountId) {
		let _ = self;
	}
	/// Ensure that creating a ticket for a given account and footprint will be successful if done
	/// immediately after this call.
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(who: &AccountId, new: Footprint);
}

impl<A, F> Consideration<A, F> for () {
	fn new(_: &A, _: F) -> Result<Self, DispatchError> {
		Ok(())
	}
	fn update(self, _: &A, _: F) -> Result<(), DispatchError> {
		Ok(())
	}
	fn drop(self, _: &A) -> Result<(), DispatchError> {
		Ok(())
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(_: &A, _: F) {}
}

/// An extension of the [`Consideration`] trait that allows for the management of tickets that may
/// represent no cost. While the [`MaybeConsideration`] still requires proper handling, it
/// introduces the ability to determine if a ticket represents no cost and can be safely forgotten
/// without any side effects.
pub trait MaybeConsideration<AccountId, Footprint>: Consideration<AccountId, Footprint> {
	/// Returns `true` if this [`Consideration`] represents a no-cost ticket and can be forgotten
	/// without any side effects.
	fn is_none(&self) -> bool;
}

impl<A, F> MaybeConsideration<A, F> for () {
	fn is_none(&self) -> bool {
		true
	}
}

macro_rules! impl_incrementable {
	($($type:ty),+) => {
		$(
			impl Incrementable for $type {
				fn increment(&self) -> Option<Self> {
					self.checked_add(1)
				}

				fn initial_value() -> Option<Self> {
					Some(0)
				}
			}
		)+
	};
}

/// A trait representing an incrementable type.
///
/// The `increment` and `initial_value` functions are fallible.
/// They should either both return `Some` with a valid value, or `None`.
pub trait Incrementable
where
	Self: Sized,
{
	/// Increments the value.
	///
	/// Returns `Some` with the incremented value if it is possible, or `None` if it is not.
	fn increment(&self) -> Option<Self>;

	/// Returns the initial value.
	///
	/// Returns `Some` with the initial value if it is available, or `None` if it is not.
	fn initial_value() -> Option<Self>;
}

impl_incrementable!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

/// Wrap a type so that is `Drop` impl is never called.
///
/// Useful when storing types like `Imbalance` which would trigger their `Drop`
/// implementation whenever they are written to storage as they are dropped after
/// being serialized.
#[derive(Default, Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo)]
pub struct NoDrop<T: Default>(T);

impl<T: Default> Drop for NoDrop<T> {
	fn drop(&mut self) {
		mem::forget(mem::take(&mut self.0))
	}
}

/// Sealed trait that marks a type with a suppressed Drop implementation.
///
/// Useful for constraining your storage items types by this bound to make
/// sure they won't run drop when stored.
pub trait SuppressedDrop: sealed::Sealed {
	/// The wrapped whose drop function is ignored.
	type Inner;

	fn new(inner: Self::Inner) -> Self;
	fn as_ref(&self) -> &Self::Inner;
	fn as_mut(&mut self) -> &mut Self::Inner;
	fn into_inner(self) -> Self::Inner;
}

impl SuppressedDrop for () {
	type Inner = ();

	fn new(inner: Self::Inner) -> Self {
		inner
	}

	fn as_ref(&self) -> &Self::Inner {
		self
	}

	fn as_mut(&mut self) -> &mut Self::Inner {
		self
	}

	fn into_inner(self) -> Self::Inner {
		self
	}
}

impl<T: Default> SuppressedDrop for NoDrop<T> {
	type Inner = T;

	fn as_ref(&self) -> &Self::Inner {
		&self.0
	}

	fn as_mut(&mut self) -> &mut Self::Inner {
		&mut self.0
	}

	fn into_inner(mut self) -> Self::Inner {
		mem::take(&mut self.0)
	}

	fn new(inner: Self::Inner) -> Self {
		Self(inner)
	}
}

mod sealed {
	pub trait Sealed {}
	impl Sealed for () {}
	impl<T: Default> Sealed for super::NoDrop<T> {}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::BoundedVec;
	use sp_core::{ConstU32, ConstU64};

	#[test]
	fn incrementable_works() {
		assert_eq!(0u8.increment(), Some(1));
		assert_eq!(1u8.increment(), Some(2));

		assert_eq!(u8::MAX.increment(), None);
	}

	#[test]
	fn linear_storage_price_works() {
		type Linear = LinearStoragePrice<ConstU64<7>, ConstU64<3>, u64>;
		let p = |count, size| Linear::convert(Footprint { count, size });

		assert_eq!(p(0, 0), 7);
		assert_eq!(p(0, 1), 7);
		assert_eq!(p(1, 0), 7);

		assert_eq!(p(1, 1), 10);
		assert_eq!(p(8, 1), 31);
		assert_eq!(p(1, 8), 31);

		assert_eq!(p(u64::MAX, u64::MAX), u64::MAX);
	}

	#[test]
	fn footprint_from_mel_works() {
		let footprint = Footprint::from_mel::<(u8, BoundedVec<u8, ConstU32<9>>)>();
		let expected_size = BoundedVec::<u8, ConstU32<9>>::max_encoded_len() as u64;
		assert_eq!(expected_size, 10);
		assert_eq!(footprint, Footprint { count: 1, size: expected_size + 1 });

		let footprint = Footprint::from_mel::<(u8, BoundedVec<u8, ConstU32<999>>)>();
		let expected_size = BoundedVec::<u8, ConstU32<999>>::max_encoded_len() as u64;
		assert_eq!(expected_size, 1001);
		assert_eq!(footprint, Footprint { count: 1, size: expected_size + 1 });
	}
}
