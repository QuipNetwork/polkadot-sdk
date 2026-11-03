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

//! Traits for creating fungible assets.
//!
//! This module initially exists to support lifetime functionality in the
//! [`crate::traits::tokens::fungible::ItemOf`] adapter for [`crate::traits::tokens::fungibles`].

use super::Inspect;
use sp_runtime::DispatchResult;

/// Trait for providing the ability to create a new fungible asset.
pub trait Create<AccountId>: Inspect<AccountId> {
	/// Create a new fungible asset.
	///
	/// - `admin`: The account that will be set as the admin of the asset.
	/// - `is_sufficient`: If `true`, the asset is sufficient and an account can exist with a zero
	///   balance. If `false`, the asset is non-sufficient and accounts must have a minimum balance.
	/// - `min_balance`: The minimum balance required for non-sufficient assets.
	fn create(admin: AccountId, is_sufficient: bool, min_balance: Self::Balance) -> DispatchResult;
}
