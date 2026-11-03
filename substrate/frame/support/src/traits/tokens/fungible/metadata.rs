// This file is part of Substrate.
//
// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0
//
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

//! Inspect and Mutate traits for fungible metadata.
//!
//! This module initially exists to support metadata functionality in the
//! [`crate::traits::tokens::fungible::ItemOf`] adapter for [`crate::traits::tokens::fungibles`].

use crate::dispatch::DispatchResult;
use alloc::vec::Vec;

/// Trait for inspecting fungible token metadata.
pub trait Inspect<AccountId>: super::Inspect<AccountId> {
	/// Returns the name of the token.
	fn name() -> Vec<u8>;
	/// Returns the ticker symbol of the token.
	fn symbol() -> Vec<u8>;
	/// Returns the number of decimals this asset uses to represent one unit.
	fn decimals() -> u8;
}

/// Trait for mutating fungible token metadata.
pub trait Mutate<AccountId>: Inspect<AccountId> {
	/// Set the name, symbol and decimals for the token.
	///
	/// - `from`: The account of the asset's owner from which the updated deposit will be reserved.
	/// - `name`: The new name.
	/// - `symbol`: The new ticker symbol.
	/// - `decimals`: The new number of decimals this asset uses to represent one unit.
	fn set(from: &AccountId, name: Vec<u8>, symbol: Vec<u8>, decimals: u8) -> DispatchResult;
}
