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

//! Tests for the `eth_estimate_gas` short-circuit fast path.

use crate::{
	EthTransactError, Pallet, RUNTIME_PALLETS_ADDR,
	address::AddressMapper,
	evm::{
		AccessListEntry, AuthorizationListEntry, Bytes, GenericTransaction, StateOverride,
		StateOverrideSet,
	},
	state_overrides::apply_state_overrides,
	test_utils::{ALICE_ADDR, BOB, BOB_ADDR, CHARLIE_ADDR},
	tests::{Config, ExtBuilder, Test, test_utils::place_contract},
};
use frame_support::traits::fungible::Mutate;
use sp_core::{H256, U256};

fn simple_transfer_tx() -> GenericTransaction {
	GenericTransaction {
		from: Some(ALICE_ADDR),
		to: Some(CHARLIE_ADDR),
		value: Some(U256::from(1_000_000)),
		..Default::default()
	}
}

#[test]
fn is_simple_transfer_classifies_fields() {
	let cases: &[(&str, GenericTransaction, bool)] = &[
		("eoa-to-eoa, empty input", simple_transfer_tx(), true),
		(
			"contract creation (to = None)",
			GenericTransaction { to: None, ..simple_transfer_tx() },
			false,
		),
		(
			"runtime pallets address",
			GenericTransaction { to: Some(RUNTIME_PALLETS_ADDR), ..simple_transfer_tx() },
			false,
		),
		(
			"non-empty input",
			GenericTransaction { input: vec![0x01].into(), ..simple_transfer_tx() },
			false,
		),
		(
			"access list",
			GenericTransaction {
				access_list: Some(vec![AccessListEntry {
					address: BOB_ADDR,
					storage_keys: vec![H256::zero()],
				}]),
				..simple_transfer_tx()
			},
			false,
		),
		(
			"empty access list",
			GenericTransaction { access_list: Some(vec![]), ..simple_transfer_tx() },
			true,
		),
		(
			"authorization list",
			GenericTransaction {
				authorization_list: vec![AuthorizationListEntry {
					chain_id: U256::zero(),
					address: BOB_ADDR,
					nonce: U256::zero(),
					y_parity: U256::zero(),
					r: U256::zero(),
					s: U256::zero(),
				}],
				..simple_transfer_tx()
			},
			false,
		),
		(
			"blob hashes",
			GenericTransaction {
				blob_versioned_hashes: vec![H256::zero()],
				..simple_transfer_tx()
			},
			false,
		),
		(
			"blobs",
			GenericTransaction { blobs: vec![Bytes::from(vec![0u8; 32])], ..simple_transfer_tx() },
			false,
		),
		(
			"max fee per blob gas",
			GenericTransaction {
				max_fee_per_blob_gas: Some(U256::from(1)),
				..simple_transfer_tx()
			},
			false,
		),
	];
	ExtBuilder::default().build().execute_with(|| {
		for (name, tx, expected) in cases {
			assert_eq!(Pallet::<Test>::is_simple_transfer(tx), *expected, "case: {name}");
		}
	});
}

#[test]
fn is_simple_transfer_observes_applied_state_overrides() {
	ExtBuilder::default().build().execute_with(|| {
		assert!(Pallet::<Test>::is_simple_transfer(&simple_transfer_tx()));

		// Make CHARLIE_ADDR a contract via state override.
		let mut overrides = StateOverrideSet::default();
		overrides.0.insert(
			CHARLIE_ADDR,
			StateOverride { code: Some(Bytes::from(vec![0xfeu8; 32])), ..Default::default() },
		);
		apply_state_overrides::<Test>(overrides).unwrap();

		assert!(!Pallet::<Test>::is_simple_transfer(&simple_transfer_tx()));
	});
}

#[test]
fn is_simple_transfer_rejects_contract_destination() {
	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, H256::repeat_byte(0xab));
		let tx = GenericTransaction { to: Some(BOB_ADDR), ..simple_transfer_tx() };
		assert!(!Pallet::<Test>::is_simple_transfer(&tx));
	});
}

#[test]
fn eth_estimate_gas_short_circuits_simple_transfer() {
	ExtBuilder::default().build().execute_with(|| {
		let alice = <Test as Config>::AddressMapper::to_account_id(&ALICE_ADDR);
		let _ = <Test as Config>::Currency::set_balance(&alice, u64::MAX as u128);

		let estimate = Pallet::<Test>::eth_estimate_gas(simple_transfer_tx(), None, None)
			.expect("simple transfer should be estimable");
		assert!(!estimate.is_zero(), "simple-transfer estimate must be non-zero");
	});
}

#[test]
fn eth_estimate_gas_short_circuit_errors_when_value_exceeds_balance() {
	ExtBuilder::default().build().execute_with(|| {
		let err = Pallet::<Test>::eth_estimate_gas(simple_transfer_tx(), None, None)
			.expect_err("transfer with empty balance must error");
		match err {
			EthTransactError::Message(msg) => {
				assert!(msg.contains("insufficient funds"), "unexpected error message: {msg}")
			},
			other => panic!("expected EthTransactError::Message, got {other:?}"),
		}
	});
}

#[test]
fn eth_estimate_gas_does_not_leak_state_overrides() {
	ExtBuilder::default().build().execute_with(|| {
		let alice = <Test as Config>::AddressMapper::to_account_id(&ALICE_ADDR);
		let _ = <Test as Config>::Currency::set_balance(&alice, u64::MAX as u128);

		// Override uninvolved accounts so the estimate still succeeds: BOB's balance and CHARLIE's
		// nonce. Both are applied inside rolled-back transactions (the classification probe and the
		// dry runs), so neither must survive the call in committed storage.
		let bob_balance_before = Pallet::<Test>::evm_balance(&BOB_ADDR);
		let charlie = <Test as Config>::AddressMapper::to_account_id(&CHARLIE_ADDR);
		let charlie_nonce_before = frame_system::Pallet::<Test>::account_nonce(&charlie);

		let mut overrides = StateOverrideSet::default();
		overrides.0.insert(
			BOB_ADDR,
			StateOverride { balance: Some(U256::from(u64::MAX)), ..Default::default() },
		);
		overrides.0.insert(
			CHARLIE_ADDR,
			StateOverride { nonce: Some(U256::from(99u8)), ..Default::default() },
		);
		Pallet::<Test>::eth_estimate_gas(simple_transfer_tx(), None, Some(overrides))
			.expect("simple transfer should be estimable");

		assert_eq!(
			Pallet::<Test>::evm_balance(&BOB_ADDR),
			bob_balance_before,
			"balance override must not leak into committed state",
		);
		assert_eq!(
			frame_system::Pallet::<Test>::account_nonce(&charlie),
			charlie_nonce_before,
			"nonce override must not leak into committed state",
		);
	});
}
