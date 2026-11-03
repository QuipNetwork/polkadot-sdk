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

//! Multi-block migrations for the recovery pallet.

use frame_support::traits::StorageVersion;

pub mod v0;
pub mod v1;

/// Storage layout version of the pallet.
pub const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

/// A unique identifier for the migrations of this pallet.
pub const PALLET_MIGRATIONS_ID: &[u8; 18] = b"pallet-recover-mbm";
