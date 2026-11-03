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

//! Tests for the `#[stored]` macro.

use codec::{Codec, MaxEncodedLen};
use core::fmt::Debug;
use frame_support::stored;
use scale_info::TypeInfo;

pub trait Config {
	type Balance: Clone + PartialEq + Eq + Debug + TypeInfo + Codec + MaxEncodedLen;
	type AccountId: Clone + PartialEq + Eq + Debug + TypeInfo + Codec + MaxEncodedLen;
}

// This type itself doesn't implement the requirement to be stored.
// but the associated types in Config does.
struct NotStored;

impl Config for NotStored {
	type Balance = u8;
	type AccountId = u64;
}

pub trait SimpleConfig {
	type Any: Clone + PartialEq + Eq + Debug + TypeInfo + Codec + MaxEncodedLen;
}

#[stored]
pub struct BasicData<T: SimpleConfig> {
	pub f1: T::Any,
	pub f2: T::Any,
}

#[stored]
pub struct AccountData<T: Config> {
	pub free: T::Balance,
	pub reserved: T::Balance,
	pub frozen: T::Balance,
	pub phantom: core::marker::PhantomData<T>,
}

#[stored]
pub enum Status<V, T: Config> {
	Active { account: T::AccountId, value: V },
	Inactive,
	Pending(T::Balance),
}

/// Helper function to ensure types implement all required storage traits.
fn ensure_storable<T: Clone + PartialEq + Eq + Debug + TypeInfo + Codec + MaxEncodedLen>() {}

impl SimpleConfig for NotStored {
	type Any = u32;
}

#[test]
fn test_stored_struct_implements_required_traits() {
	ensure_storable::<BasicData<NotStored>>();
	ensure_storable::<AccountData<NotStored>>();
}

#[test]
fn test_stored_enum_implements_required_traits() {
	ensure_storable::<Status<u128, NotStored>>();
}

#[stored]
pub struct DeriveWhereNotNeeded<T>(T);

#[test]
fn test_derive_where_not_needed_storable() {
	ensure_storable::<DeriveWhereNotNeeded<u8>>();
}

#[stored]
pub struct NoGenerics(u8);

#[test]
fn test_no_generics_storable() {
	ensure_storable::<NoGenerics>();
}

// when not skipped the type parameter T is included in the type params type info.
#[test]
fn no_skip_type_params() {
	use scale_info::TypeInfo;

	#[stored(no_skip_type_params)]
	pub struct NoSkip<T>(T);

	assert_eq!(
		format!("{:?}", NoSkip::<u8>::type_info()),
		"Type { \
		path: Path { segments: [\"stored\", \"NoSkip\"] }, \
		type_params: [TypeParameter { name: \"T\", ty: Some(TypeId(0x0596b48cc04376e64d5c788c2aa46bdb)) }], \
		type_def: Composite(TypeDefComposite { fields: [Field { name: None, ty: TypeId(0x0596b48cc04376e64d5c788c2aa46bdb), type_name: Some(\"T\"), docs: [] }] }), \
		docs: [] }"
	);
}
