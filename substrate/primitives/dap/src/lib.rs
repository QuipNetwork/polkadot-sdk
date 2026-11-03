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

//! Primitives for the Dynamic Allocation Pool (DAP).
//!
//! Shared across `pallet-dap` and related consumers to ensure
//! agreement on the DAP buffer account derivation.

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::PalletId;

/// The [`PalletId`] used to represent the central DAP pallet.
pub const DAP_PALLET_ID: PalletId = PalletId(*b"dap/buff");

/// Sub-account identifier used to derive the DAP staging account.
pub const DAP_STAGING_ACCOUNT_ID: &[u8] = b"staging";
