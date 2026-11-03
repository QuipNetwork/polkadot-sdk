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

//! Provides host <-> runtime FFI marshalling strategy newtype wrappers
//! for defining runtime interfaces.

use crate::{
	util::{pack_ptr_and_len, unpack_ptr_and_len},
	RIType,
};

#[cfg(not(substrate_runtime))]
use crate::host::*;

#[cfg(substrate_runtime)]
use crate::wasm::*;

#[cfg(not(substrate_runtime))]
use sp_wasm_interface::{FunctionContext, Pointer, Result};

#[cfg(not(substrate_runtime))]
use alloc::{format, string::String};

use alloc::vec::Vec;
use core::{any::type_name, marker::PhantomData};

/// Pass a value into the host by a thin pointer.
///
/// This casts the value into a `&[u8]` using `AsRef<[u8]>` and passes a pointer to that byte blob
/// to the host. Then the host reads `N` bytes from that address into an `[u8; N]`, converts it
/// into target type using `From<[u8; N]>` and passes it into the host function by a copy.
///
/// Use [`PassPointerAndRead`] if you want to have the host function accept a reference type
/// on the host side or if you'd like to avoid the extra copy.
///
/// Raw FFI type: `u32` (a pointer)
pub struct PassPointerAndReadCopy<T, const N: usize>(PhantomData<(T, [u8; N])>);

impl<T, const N: usize> RIType for PassPointerAndReadCopy<T, N> {
	type FFIType = u32;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a, T, const N: usize> FromFFIValue<'a> for PassPointerAndReadCopy<T, N>
where
	T: From<[u8; N]> + Copy,
{
	type Owned = T;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let mut out = [0; N];
		context.read_memory_into(Pointer::new(arg), &mut out)?;
		Ok(T::from(out))
	}

	#[inline]
	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		*owned
	}
}

#[cfg(substrate_runtime)]
impl<T, const N: usize> IntoFFIValue for PassPointerAndReadCopy<T, N>
where
	T: AsRef<[u8]>,
{
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		// Using an 'assert' instead of a 'T: AsRef<[u8; N]>` bound since a '[u8; N]' *doesn't*
		// implement it.
		assert_eq!(value.as_ref().len(), N);
		(value.as_ref().as_ptr() as u32, ())
	}
}

/// Pass a value into the host by a thin pointer.
///
/// This casts the value into a `&[u8]` using `AsRef<[u8]>` and passes a pointer to that byte blob
/// to the host. Then the host reads `N` bytes from that address into an `[u8; N]`, converts it
/// into target type using `From<[u8; N]>` and passes it into the host function by a reference.
///
/// This can only be used with reference types (e.g. `&[u8; 32]`). Use [`PassPointerAndReadCopy`]
/// if you want to have the host function accept a non-reference type on the host side.
///
/// Raw FFI type: `u32` (a pointer)
pub struct PassPointerAndRead<T, const N: usize>(PhantomData<(T, [u8; N])>);

impl<'a, T, const N: usize> RIType for PassPointerAndRead<&'a T, N> {
	type FFIType = u32;
	type Inner = &'a T;
}

#[cfg(not(substrate_runtime))]
impl<'a, T, const N: usize> FromFFIValue<'a> for PassPointerAndRead<&'a T, N>
where
	T: From<[u8; N]>,
{
	type Owned = T;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let mut out = [0; N];
		context.read_memory_into(Pointer::new(arg), &mut out)?;
		Ok(T::from(out))
	}

	#[inline]
	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&*owned
	}
}

#[cfg(substrate_runtime)]
impl<'a, T, const N: usize> IntoFFIValue for PassPointerAndRead<&'a T, N>
where
	T: AsRef<[u8]>,
{
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		assert_eq!(value.as_ref().len(), N);
		(value.as_ref().as_ptr() as u32, ())
	}
}

/// Pass a value into the host by a fat pointer.
///
/// This casts the value into a `&[u8]` and passes a pointer to that byte blob and its length
/// to the host. Then the host reads that blob and converts it into an owned type and passes it
/// (either as an owned type or as a reference) to the host function.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct PassFatPointerAndRead<T>(PhantomData<T>);

impl<T> RIType for PassFatPointerAndRead<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a> FromFFIValue<'a> for PassFatPointerAndRead<&'a [u8]> {
	type Owned = Vec<u8>;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		context.read_memory(Pointer::new(ptr), len)
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&*owned
	}
}

#[cfg(not(substrate_runtime))]
impl<'a> FromFFIValue<'a> for PassFatPointerAndRead<&'a str> {
	type Owned = String;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		let vec = context.read_memory(Pointer::new(ptr), len)?;
		String::from_utf8(vec).map_err(|_| "could not parse '&str' when marshalling hostcall's arguments through the FFI boundary: the string is not valid UTF-8".into())
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&*owned
	}
}

#[cfg(not(substrate_runtime))]
impl<'a> FromFFIValue<'a> for PassFatPointerAndRead<Vec<u8>> {
	type Owned = Vec<u8>;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		<PassFatPointerAndRead<&[u8]> as FromFFIValue>::from_ffi_value(context, arg)
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		core::mem::take(owned)
	}
}

#[cfg(substrate_runtime)]
impl<T> IntoFFIValue for PassFatPointerAndRead<T>
where
	T: AsRef<[u8]>,
{
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		let value = value.as_ref();
		(pack_ptr_and_len(value.as_ptr() as u32, value.len() as u32), ())
	}
}

/// Pass an `Option` of a value into the host by a fat pointer.
///
/// Behaves like [`PassFatPointerAndRead`] for the `Some` case, with `None` encoded as the
/// sentinel fat pointer `(ptr = 0, len = 0)`. This is unambiguous: the runtime side calls
/// `as_ptr()` on a slice, which is documented never to return null even for empty slices,
/// so a real `Some(&[])` always carries a non-zero pointer.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct PassFatPointerAndReadOption<T>(PhantomData<T>);

impl<T> RIType for PassFatPointerAndReadOption<T> {
	type FFIType = u64;
	type Inner = Option<T>;
}

#[cfg(not(substrate_runtime))]
impl<'a> FromFFIValue<'a> for PassFatPointerAndReadOption<&'a [u8]> {
	type Owned = Option<Vec<u8>>;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		if ptr == 0 {
			Ok(None)
		} else {
			context.read_memory(Pointer::new(ptr), len).map(Some)
		}
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		owned.as_deref()
	}
}

#[cfg(substrate_runtime)]
impl<T> IntoFFIValue for PassFatPointerAndReadOption<T>
where
	T: AsRef<[u8]>,
{
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		match value {
			Some(value) => {
				let value = value.as_ref();
				(pack_ptr_and_len(value.as_ptr() as u32, value.len() as u32), ())
			},
			None => (pack_ptr_and_len(0, 0), ()),
		}
	}
}

/// Pass a value into the host by a fat pointer, writing it back after the host call ends.
///
/// This casts the value into a `&mut [u8]` and passes a pointer to that byte blob and its length
/// to the host. Then the host reads that blob and converts it into an owned type and passes it
/// as a mutable reference to the host function. After the host function finishes the byte blob
/// is written back into the guest memory.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct PassFatPointerAndReadWrite<T>(PhantomData<T>);

impl<T> RIType for PassFatPointerAndReadWrite<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a> FromFFIValue<'a> for PassFatPointerAndReadWrite<&'a mut [u8]> {
	type Owned = Vec<u8>;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		context.read_memory(Pointer::new(ptr), len)
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&mut *owned
	}

	fn write_back_into_runtime(
		value: Self::Owned,
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<()> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		assert_eq!(len as usize, value.len());
		context.write_memory(Pointer::new(ptr), &value)
	}
}

#[cfg(substrate_runtime)]
impl<'a> IntoFFIValue for PassFatPointerAndReadWrite<&'a mut [u8]> {
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		(pack_ptr_and_len(value.as_ptr() as u32, value.len() as u32), ())
	}
}

/// Pass a pointer into the host by a fat pointer, writing it back after the host call ends.
///
/// This casts the value into a `&mut [u8]` and passes a pointer to that byte blob and its length
/// to the host. The host *doesn't* read from this and instead creates a zero-initialized buffer
/// as a mutable reference to the host function. After the host function finishes the byte blob
/// is written back into the guest memory.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct PassFatPointerAndWrite<T>(PhantomData<T>);

impl<T> RIType for PassFatPointerAndWrite<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a> FromFFIValue<'a> for PassFatPointerAndWrite<&'a mut [u8]> {
	type Owned = Vec<u8>;

	fn from_ffi_value(
		_context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (_ptr, len) = unpack_ptr_and_len(arg);
		Ok(alloc::vec![0u8; len as usize])
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&mut *owned
	}

	fn write_back_into_runtime(
		value: Self::Owned,
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<()> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		assert_eq!(len as usize, value.len());
		context.write_memory(Pointer::new(ptr), &value)
	}
}

#[cfg(substrate_runtime)]
impl<'a> IntoFFIValue for PassFatPointerAndWrite<&'a mut [u8]> {
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		(pack_ptr_and_len(value.as_ptr() as u32, value.len() as u32), ())
	}
}

/// Pass a pointer into the host and write to it after the host call ends.
///
/// This casts a given type into `&mut [u8]` using `AsMut<[u8]>` and passes a pointer to
/// that byte slice into the host. The host *doesn't* read from this and instead creates
/// a default instance of type `T` and passes it as a `&mut T` into the host function
/// implementation. After the host function finishes this value is then cast into a `&[u8]` using
/// `AsRef<[u8]>` and written back into the guest memory.
///
/// Raw FFI type: `u32` (a pointer)
pub struct PassPointerAndWrite<T, const N: usize>(PhantomData<(T, [u8; N])>);

impl<T, const N: usize> RIType for PassPointerAndWrite<T, N> {
	type FFIType = u32;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a, T, const N: usize> FromFFIValue<'a> for PassPointerAndWrite<&'a mut T, N>
where
	T: Default + AsRef<[u8]>,
{
	type Owned = T;

	fn from_ffi_value(
		_context: &mut dyn FunctionContext,
		_arg: Self::FFIType,
	) -> Result<Self::Owned> {
		Ok(T::default())
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&mut *owned
	}

	fn write_back_into_runtime(
		value: Self::Owned,
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<()> {
		let value = value.as_ref();
		assert_eq!(value.len(), N);
		context.write_memory(Pointer::new(arg), value)
	}
}

#[cfg(substrate_runtime)]
impl<'a, T, const N: usize> IntoFFIValue for PassPointerAndWrite<&'a mut T, N>
where
	T: AsMut<[u8]>,
{
	type Destructor = ();

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		let value = value.as_mut();
		assert_eq!(value.len(), N);
		(value.as_ptr() as u32, ())
	}
}

/// Pass a `T` into the host using the SCALE codec.
///
/// This encodes a `T` into a `Vec<u8>` using the SCALE codec and then
/// passes a pointer to that byte blob and its length to the host,
/// which then reads it and decodes back into `T`.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct PassFatPointerAndDecode<T>(PhantomData<T>);

impl<T> RIType for PassFatPointerAndDecode<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a, T: codec::Decode> FromFFIValue<'a> for PassFatPointerAndDecode<T> {
	type Owned = Option<T>;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		let vec = context.read_memory(Pointer::new(ptr), len)?;
		T::decode(&mut &vec[..]).map_err(|error| format!(
			"could not SCALE-decode '{}' when marshalling hostcall's arguments through the FFI boundary: {error}",
			type_name::<T>())
		).map(Some)
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		owned.take().expect("this is called only once and is never 'None'")
	}
}

#[cfg(substrate_runtime)]
impl<T: codec::Encode> IntoFFIValue for PassFatPointerAndDecode<T> {
	type Destructor = Vec<u8>;

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		let data = value.encode();
		(pack_ptr_and_len(data.as_ptr() as u32, data.len() as u32), data)
	}
}

/// Pass a `&[T]` into the host using the SCALE codec.
///
/// This encodes a `&[T]` into a `Vec<u8>` using the SCALE codec and then
/// passes a pointer to that byte blob and its length to the host,
/// which then reads it and decodes back into `Vec<T>` and passes
/// a reference to that (as `&[T]`) into the host function.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct PassFatPointerAndDecodeSlice<T>(PhantomData<T>);

impl<T> RIType for PassFatPointerAndDecodeSlice<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a, T: codec::Decode> FromFFIValue<'a> for PassFatPointerAndDecodeSlice<&'a [T]> {
	type Owned = Vec<T>;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		let (ptr, len) = unpack_ptr_and_len(arg);
		let vec = context.read_memory(Pointer::new(ptr), len)?;
		<Vec::<T> as codec::Decode>::decode(&mut &vec[..]).map_err(|error| format!(
			"could not SCALE-decode '{}' when marshalling hostcall's arguments through the FFI boundary: {error}",
			type_name::<Vec<T>>()
		))
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		&*owned
	}
}

#[cfg(substrate_runtime)]
impl<'a, T: codec::Encode> IntoFFIValue for PassFatPointerAndDecodeSlice<&'a [T]> {
	type Destructor = Vec<u8>;

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		let data = codec::Encode::encode(value);
		(pack_ptr_and_len(data.as_ptr() as u32, data.len() as u32), data)
	}
}

/// A trait signifying a primitive type.
trait Primitive: Copy {}

impl Primitive for u8 {}
impl Primitive for u16 {}
impl Primitive for u32 {}
impl Primitive for u64 {}

impl Primitive for i8 {}
impl Primitive for i16 {}
impl Primitive for i32 {}
impl Primitive for i64 {}

/// Pass `T` through the FFI boundary by first converting it to `U` in the runtime, and then
/// converting it back to `T` on the host's side.
///
/// Raw FFI type: same as `U`'s FFI type
pub struct PassAs<T, U>(PhantomData<(T, U)>);

impl<T, U> RIType for PassAs<T, U>
where
	U: RIType,
{
	type FFIType = <U as RIType>::FFIType;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<'a, T, U> FromFFIValue<'a> for PassAs<T, U>
where
	U: RIType + FromFFIValue<'a> + Primitive,
	T: TryFrom<<U as FromFFIValue<'a>>::Owned> + Copy,
{
	type Owned = T;

	fn from_ffi_value(
		context: &mut dyn FunctionContext,
		arg: Self::FFIType,
	) -> Result<Self::Owned> {
		<U as FromFFIValue>::from_ffi_value(context, arg).and_then(|value| value.try_into()
			.map_err(|_| format!(
				"failed to convert '{}' (passed as '{}') into '{}' when marshalling hostcall's arguments through the FFI boundary",
				type_name::<U>(),
				type_name::<Self::FFIType>(),
				type_name::<Self::Owned>()
			)))
	}

	fn take_from_owned(owned: &'a mut Self::Owned) -> Self::Inner {
		*owned
	}
}

#[cfg(substrate_runtime)]
impl<T, U> IntoFFIValue for PassAs<T, U>
where
	U: RIType + IntoFFIValue + Primitive,
	U::Inner: From<T>,
	T: Copy,
{
	type Destructor = <U as IntoFFIValue>::Destructor;

	fn into_ffi_value(value: &mut Self::Inner) -> (Self::FFIType, Self::Destructor) {
		let mut value = U::Inner::from(*value);
		<U as IntoFFIValue>::into_ffi_value(&mut value)
	}
}

/// Return `T` through the FFI boundary by first converting it to `U` on the host's side, and then
/// converting it back to `T` in the runtime.
///
/// Raw FFI type: same as `U`'s FFI type
pub struct ReturnAs<T, U>(PhantomData<(T, U)>);

impl<T, U> RIType for ReturnAs<T, U>
where
	U: RIType,
{
	type FFIType = <U as RIType>::FFIType;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<T, U> IntoFFIValue for ReturnAs<T, U>
where
	U: RIType + IntoFFIValue + Primitive,
	<U as RIType>::Inner: From<Self::Inner>,
{
	fn into_ffi_value(
		value: Self::Inner,
		context: &mut dyn FunctionContext,
	) -> Result<Self::FFIType> {
		let value: <U as RIType>::Inner = value.into();
		<U as IntoFFIValue>::into_ffi_value(value, context)
	}
}

#[cfg(substrate_runtime)]
impl<T, U> FromFFIValue for ReturnAs<T, U>
where
	U: RIType + FromFFIValue + Primitive,
	Self::Inner: TryFrom<U::Inner>,
{
	fn from_ffi_value(arg: Self::FFIType) -> Self::Inner {
		let value = <U as FromFFIValue>::from_ffi_value(arg);
		match Self::Inner::try_from(value) {
			Ok(value) => value,
			Err(_) => {
				panic!(
					"failed to convert '{}' (passed as '{}') into a '{}' when marshalling a hostcall's return value through the FFI boundary",
					type_name::<U::Inner>(),
					type_name::<Self::FFIType>(),
					type_name::<Self::Inner>()
				);
			},
		}
	}
}

/// Return `T` through the FFI boundary by first converting it to `U` and then to `V` on the
/// host's side, and then converting it back to `U` and then to `T` in the runtime.
///
/// This is useful to pass types when the conversion to/from FFI type cannot be implemented
/// directly, e.g. because of the orphan rule.
///
/// Raw FFI type: same as `V`'s FFI type
pub struct ConvertAndReturnAs<T, U, V>(PhantomData<(T, U, V)>);

impl<T, U, V> RIType for ConvertAndReturnAs<T, U, V>
where
	V: RIType,
{
	type FFIType = <V as RIType>::FFIType;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<T, U, V> IntoFFIValue for ConvertAndReturnAs<T, U, V>
where
	V: RIType + IntoFFIValue + Primitive,
	<V as RIType>::Inner: From<U>,
	U: From<Self::Inner>,
{
	fn into_ffi_value(
		value: Self::Inner,
		context: &mut dyn FunctionContext,
	) -> Result<Self::FFIType> {
		let value: U = value.into();
		let value: <V as RIType>::Inner = value.into();
		<V as IntoFFIValue>::into_ffi_value(value, context)
	}
}

#[cfg(substrate_runtime)]
impl<T, U, V> FromFFIValue for ConvertAndReturnAs<T, U, V>
where
	V: RIType + FromFFIValue + Primitive,
	U: TryFrom<V::Inner>,
	Self::Inner: From<U>,
{
	fn from_ffi_value(arg: Self::FFIType) -> Self::Inner {
		let value = <V as FromFFIValue>::from_ffi_value(arg);
		let value = match U::try_from(value) {
			Ok(value) => value,
			Err(_) => {
				panic!(
					"failed to convert '{}' (passed as '{}') into a intermediate type '{}' when marshalling a hostcall's return value through the FFI boundary",
					type_name::<V::Inner>(),
					type_name::<Self::FFIType>(),
					type_name::<U>()
				);
			},
		};
		value.into()
	}
}

/// (DEPRECATED) Return `T` as a blob of bytes into the runtime.
///
/// Uses `T::AsRef<[u8]>` to cast `T` into a `&[u8]`, allocates runtime memory
/// using the legacy allocator, copies the slice into the runtime memory, and
/// returns a pointer to it.
///
/// THIS STRATEGY IS DEPRECATED; DO NOT USE FOR NEW HOST FUNCTIONS!
///
/// Ideally use a mutable slice to return data to the guest, for example using
/// the [`PassPointerAndWrite`] strategy.
///
/// Raw FFI type: `u32` (a pointer to the byte blob)
pub struct AllocateAndReturnPointer<T, const N: usize>(PhantomData<(T, [u8; N])>);

impl<T, const N: usize> RIType for AllocateAndReturnPointer<T, N> {
	type FFIType = u32;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<T, const N: usize> IntoFFIValue for AllocateAndReturnPointer<T, N>
where
	T: AsRef<[u8]>,
{
	fn into_ffi_value(
		value: Self::Inner,
		context: &mut dyn FunctionContext,
	) -> Result<Self::FFIType> {
		let value = value.as_ref();
		assert_eq!(
			value.len(),
			N,
			"expected the byte blob to be {N} bytes long, is {} bytes when returning '{}' from a host function",
			value.len(),
			type_name::<T>()
		);

		let addr = context.allocate_memory(value.len() as u32)?;
		context.write_memory(addr, value)?;
		Ok(addr.into())
	}
}

#[cfg(substrate_runtime)]
impl<T: codec::Decode, const N: usize> FromFFIValue for AllocateAndReturnPointer<T, N>
where
	T: From<[u8; N]>,
{
	fn from_ffi_value(arg: Self::FFIType) -> Self::Inner {
		// SAFETY: This memory was allocated by the host allocator with the exact
		// capacity needed, so it's safe to make a `Vec` out of it.
		let value = unsafe { Vec::from_raw_parts(arg as *mut u8, N, N) };

		// SAFETY: Reading a `[u8; N]` from a `&[u8]` which is at least `N` elements long is safe.
		let array = unsafe { *(value.as_ptr() as *const [u8; N]) };
		T::from(array)
	}
}

/// (DEPRECATED) Return `T` as a blob of bytes into the runtime.
///
/// Uses `T::AsRef<[u8]>` to cast `T` into a `&[u8]`, allocates runtime memory
/// using the legacy allocator, copies the slice into the runtime memory, and
/// returns a pointer to it.
///
/// THIS STRATEGY IS DEPRECATED; DO NOT USE FOR NEW HOST FUNCTIONS!
///
/// Ideally use a mutable slice to return data to the guest, for example using
/// the [`PassPointerAndWrite`] strategy.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct AllocateAndReturnFatPointer<T>(PhantomData<T>);

impl<T> RIType for AllocateAndReturnFatPointer<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<T> IntoFFIValue for AllocateAndReturnFatPointer<T>
where
	T: AsRef<[u8]>,
{
	fn into_ffi_value(
		value: Self::Inner,
		context: &mut dyn FunctionContext,
	) -> Result<Self::FFIType> {
		let value = value.as_ref();
		let ptr = context.allocate_memory(value.len() as u32)?;
		context.write_memory(ptr, &value)?;
		Ok(pack_ptr_and_len(ptr.into(), value.len() as u32))
	}
}

#[cfg(substrate_runtime)]
impl<T> FromFFIValue for AllocateAndReturnFatPointer<T>
where
	T: From<Vec<u8>>,
{
	fn from_ffi_value(arg: Self::FFIType) -> Self::Inner {
		let (ptr, len) = unpack_ptr_and_len(arg);
		let len = len as usize;
		let vec = if len == 0 {
			Vec::new()
		} else {
			// SAFETY: This memory was allocated by the host allocator with the exact
			// capacity needed, so it's safe to make a `Vec` out of it.
			unsafe { Vec::from_raw_parts(ptr as *mut u8, len, len) }
		};

		T::from(vec)
	}
}

/// (DEPRECATED) Return `T` into the runtime using the SCALE codec.
///
/// Encodes `T` using the SCALE codec, allocates runtime memory using the legacy
/// allocator, copies the encoded payload into the runtime memory, and returns
/// a fat pointer to it.
///
/// THIS STRATEGY IS DEPRECATED; DO NOT USE FOR NEW HOST FUNCTIONS!
///
/// Ideally use a mutable slice to return data to the guest, for example using
/// the [`PassPointerAndWrite`] strategy.
///
/// Raw FFI type: `u64` (a fat pointer; upper 32 bits is the size, lower 32 bits is the pointer)
pub struct AllocateAndReturnByCodec<T>(PhantomData<T>);

impl<T> RIType for AllocateAndReturnByCodec<T> {
	type FFIType = u64;
	type Inner = T;
}

#[cfg(not(substrate_runtime))]
impl<T: codec::Encode> IntoFFIValue for AllocateAndReturnByCodec<T> {
	fn into_ffi_value(value: T, context: &mut dyn FunctionContext) -> Result<Self::FFIType> {
		let vec = value.encode();
		let ptr = context.allocate_memory(vec.len() as u32)?;
		context.write_memory(ptr, &vec)?;
		Ok(pack_ptr_and_len(ptr.into(), vec.len() as u32))
	}
}

#[cfg(substrate_runtime)]
impl<T: codec::Decode> FromFFIValue for AllocateAndReturnByCodec<T> {
	fn from_ffi_value(arg: Self::FFIType) -> Self::Inner {
		let (ptr, len) = unpack_ptr_and_len(arg);
		let len = len as usize;

		let encoded = if len == 0 {
			bytes::Bytes::new()
		} else {
			// SAFETY: This memory was allocated by the host allocator with the exact
			// capacity needed, so it's safe to make a `Vec` out of it.
			bytes::Bytes::from(unsafe { Vec::from_raw_parts(ptr as *mut u8, len, len) })
		};

		match codec::decode_from_bytes(encoded) {
			Ok(value) => value,
			Err(error) => {
				panic!(
					"failed to decode '{}' when marshalling a hostcall's return value through the FFI boundary: {error}",
					type_name::<T>(),
				);
			},
		}
	}
}
