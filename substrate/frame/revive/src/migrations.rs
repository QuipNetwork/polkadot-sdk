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

/// # Multi-Block Migrations Module

/// Migrations from the old `ContractInfoOf` to the new `AccountInfoOf` storage
pub mod v1;

/// Migrations from the old `CodeInfoOf` to the new `CodeInfoOf` storage
pub mod v2;

/// Auto-map all existing accounts
pub mod v3;

/// Switch storage deposits from native currency to PGAS.
pub mod v4;

/// A unique identifier across all pallets.
const PALLET_MIGRATIONS_ID: &[u8; 17] = b"pallet-revive-mbm";
