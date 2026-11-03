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

//! Genesis tests for the DAP pallet.

use crate::mock::*;

type DapPallet = crate::Pallet<Test>;

#[test]
fn genesis_creates_buffer_account() {
	build_and_execute(true, || {
		set_default_budget_allocation();

		let buffer = DapPallet::buffer_account();
		// Buffer account should exist after genesis (created via inc_providers)
		assert!(System::account_exists(&buffer));
	});
}
