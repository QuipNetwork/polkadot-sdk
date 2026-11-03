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

//! Tests for the DAP pallet.

mod budget;
mod drip;
mod genesis;
mod on_unbalanced;

use crate::BudgetAllocationMap;
use sp_runtime::{BoundedBTreeMap, Perbill};
use sp_staking::budget::BudgetKey;

fn key(name: &[u8]) -> BudgetKey {
	BudgetKey::truncate_from(name.to_vec())
}

fn budget_map(entries: &[(&[u8], u32)]) -> BudgetAllocationMap {
	let mut map = BoundedBTreeMap::new();
	for (name, pct) in entries {
		map.try_insert(key(name), Perbill::from_percent(*pct)).unwrap();
	}
	map
}
