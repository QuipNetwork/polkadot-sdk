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

//! Genesis tests for the accumulate-and-forward pallet.

use crate::mock::*;
use frame_support::sp_runtime::traits::AccountIdConversion;

type AccumulateForwardPallet = crate::Pallet<Test>;

#[test]
fn accumulation_account_is_derived_from_pallet_id() {
	new_test_ext(true).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		let expected: u64 = AccumulateForwardPalletId::get().into_account_truncating();
		assert_eq!(accumulation_account, expected);
	});
}

#[test]
fn accumulation_account_exists_when_funded_via_balances_genesis() {
	new_test_ext(true).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		// Given: accumulation account was funded with ED in balances genesis config
		assert!(System::account_exists(&accumulation_account));
		assert_eq!(Balances::free_balance(accumulation_account), ExistentialDeposit::get());
	});
}

#[test]
fn accumulation_account_does_not_exist_when_not_funded() {
	new_test_ext(false).execute_with(|| {
		let accumulation_account = AccumulateForwardPallet::accumulation_account();
		assert!(!System::account_exists(&accumulation_account));
		assert_eq!(Balances::free_balance(accumulation_account), 0);
	});
}
