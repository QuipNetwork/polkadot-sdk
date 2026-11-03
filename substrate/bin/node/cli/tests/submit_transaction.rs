// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use codec::Decode;
use frame_system::offchain::{
	CreateAuthorizedTransaction, SendSignedTransaction, Signer, SubmitTransaction,
};
use kitchensink_runtime::{Executive, ExistentialDeposit, Indices, Runtime, UncheckedExtrinsic};
use polkadot_sdk::*;
use sp_application_crypto::AppCrypto;
use sp_core::offchain::{testing::TestTransactionPoolExt, TransactionPoolExt};
use sp_keyring::sr25519::Keyring::Alice;
use sp_keystore::{testing::MemoryKeystore, Keystore, KeystoreExt};

pub mod common;
use self::common::*;

#[test]
fn should_submit_authorized_transaction() {
	let mut t = new_test_ext(compact_code_unwrap());
	let (pool, state) = TestTransactionPoolExt::new();
	t.register_extension(TransactionPoolExt::new(pool));

	t.execute_with(|| {
		let signature =
			pallet_im_online::sr25519::AuthoritySignature::try_from(vec![0; 64]).unwrap();
		let heartbeat_data = pallet_im_online::Heartbeat {
			block_number: 1,
			session_index: 1,
			authority_index: 0,
			validators_len: 0,
		};

		let call = pallet_im_online::Call::heartbeat { heartbeat: heartbeat_data, signature };
		let xt = <Runtime as CreateAuthorizedTransaction<
			pallet_im_online::Call<Runtime>,
		>>::create_authorized_transaction(call.into());
		SubmitTransaction::<Runtime, pallet_im_online::Call<Runtime>>::submit_transaction(xt)
			.unwrap();

		assert_eq!(state.read().transactions.len(), 1)
	});
}

const PHRASE: &str = "news slush supreme milk chapter athlete soap sausage put clutch what kitten";

#[test]
fn should_submit_signed_transaction() {
	let mut t = new_test_ext(compact_code_unwrap());
	let (pool, state) = TestTransactionPoolExt::new();
	t.register_extension(TransactionPoolExt::new(pool));

	let keystore = MemoryKeystore::new();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter1", PHRASE)))
		.unwrap();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter2", PHRASE)))
		.unwrap();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter3", PHRASE)))
		.unwrap();
	t.register_extension(KeystoreExt::new(keystore));

	t.execute_with(|| {
		let results =
			Signer::<Runtime, TestAuthorityId>::all_accounts().send_signed_transaction(|_| {
				pallet_balances::Call::transfer_allow_death {
					dest: Alice.to_account_id().into(),
					value: Default::default(),
				}
			});

		let len = results.len();
		assert_eq!(len, 3);
		assert_eq!(results.into_iter().filter_map(|x| x.1.ok()).count(), len);
		assert_eq!(state.read().transactions.len(), len);
	});
}

#[test]
fn should_submit_signed_twice_from_the_same_account() {
	let mut t = new_test_ext(compact_code_unwrap());
	let (pool, state) = TestTransactionPoolExt::new();
	t.register_extension(TransactionPoolExt::new(pool));

	let keystore = MemoryKeystore::new();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter1", PHRASE)))
		.unwrap();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter2", PHRASE)))
		.unwrap();
	t.register_extension(KeystoreExt::new(keystore));

	t.execute_with(|| {
		let result =
			Signer::<Runtime, TestAuthorityId>::any_account().send_signed_transaction(|_| {
				pallet_balances::Call::transfer_allow_death {
					dest: Alice.to_account_id().into(),
					value: Default::default(),
				}
			});

		assert!(result.is_some());
		assert_eq!(state.read().transactions.len(), 1);

		// submit another one from the same account. The nonce should be incremented.
		let result =
			Signer::<Runtime, TestAuthorityId>::any_account().send_signed_transaction(|_| {
				pallet_balances::Call::transfer_allow_death {
					dest: Alice.to_account_id().into(),
					value: Default::default(),
				}
			});

		assert!(result.is_some());
		assert_eq!(state.read().transactions.len(), 2);

		// now check that the transaction nonces are not equal
		let s = state.read();
		fn nonce(tx: UncheckedExtrinsic) -> frame_system::CheckNonce<Runtime> {
			let extra = tx.0.preamble.to_signed().unwrap().2;
			extra.6
		}
		let nonce1 = nonce(UncheckedExtrinsic::decode(&mut &*s.transactions[0]).unwrap());
		let nonce2 = nonce(UncheckedExtrinsic::decode(&mut &*s.transactions[1]).unwrap());
		assert!(nonce1 != nonce2, "Transactions should have different nonces. Got: {:?}", nonce1);
	});
}

#[test]
fn should_submit_signed_twice_from_all_accounts() {
	let mut t = new_test_ext(compact_code_unwrap());
	let (pool, state) = TestTransactionPoolExt::new();
	t.register_extension(TransactionPoolExt::new(pool));

	let keystore = MemoryKeystore::new();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter1", PHRASE)))
		.unwrap();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter2", PHRASE)))
		.unwrap();
	t.register_extension(KeystoreExt::new(keystore));

	t.execute_with(|| {
		let results = Signer::<Runtime, TestAuthorityId>::all_accounts()
			.send_signed_transaction(|_| {
				pallet_balances::Call::transfer_allow_death { dest: Alice.to_account_id().into(), value: Default::default() }
			});

		let len = results.len();
		assert_eq!(len, 2);
		assert_eq!(results.into_iter().filter_map(|x| x.1.ok()).count(), len);
		assert_eq!(state.read().transactions.len(), 2);

		// submit another one from the same account. The nonce should be incremented.
		let results = Signer::<Runtime, TestAuthorityId>::all_accounts()
			.send_signed_transaction(|_| {
				pallet_balances::Call::transfer_allow_death { dest: Alice.to_account_id().into(), value: Default::default() }
			});

		let len = results.len();
		assert_eq!(len, 2);
		assert_eq!(results.into_iter().filter_map(|x| x.1.ok()).count(), len);
		assert_eq!(state.read().transactions.len(), 4);

		// now check that the transaction nonces are not equal
		let s = state.read();
		fn nonce(tx: UncheckedExtrinsic) -> frame_system::CheckNonce<Runtime> {
			let extra = tx.0.preamble.to_signed().unwrap().2;
			extra.6
		}
		let nonce1 = nonce(UncheckedExtrinsic::decode(&mut &*s.transactions[0]).unwrap());
		let nonce2 = nonce(UncheckedExtrinsic::decode(&mut &*s.transactions[1]).unwrap());
		let nonce3 = nonce(UncheckedExtrinsic::decode(&mut &*s.transactions[2]).unwrap());
		let nonce4 = nonce(UncheckedExtrinsic::decode(&mut &*s.transactions[3]).unwrap());
		assert!(
			nonce1 != nonce3,
			"Transactions should have different nonces. Got: 1st tx nonce: {:?}, 2nd nonce: {:?}", nonce1, nonce3
		);
		assert!(
			nonce2 != nonce4,
			"Transactions should have different nonces. Got: 1st tx nonce: {:?}, 2nd tx nonce: {:?}", nonce2, nonce4
		);
	});
}

#[test]
fn submitted_transaction_should_be_valid() {
	use codec::Encode;
	use sp_runtime::{
		traits::StaticLookup,
		transaction_validity::{TransactionSource, TransactionTag},
	};

	let mut t = new_test_ext(compact_code_unwrap());
	let (pool, state) = TestTransactionPoolExt::new();
	t.register_extension(TransactionPoolExt::new(pool));

	let keystore = MemoryKeystore::new();
	keystore
		.sr25519_generate_new(sr25519::AuthorityId::ID, Some(&format!("{}/hunter1", PHRASE)))
		.unwrap();
	t.register_extension(KeystoreExt::new(keystore));

	t.execute_with(|| {
		let results =
			Signer::<Runtime, TestAuthorityId>::all_accounts().send_signed_transaction(|_| {
				pallet_balances::Call::transfer_allow_death {
					dest: Alice.to_account_id().into(),
					value: Default::default(),
				}
			});
		let len = results.len();
		assert_eq!(len, 1);
		assert_eq!(results.into_iter().filter_map(|x| x.1.ok()).count(), len);
	});

	// check that transaction is valid, but reset environment storage,
	// since CreateTransaction increments the nonce
	let tx0 = state.read().transactions[0].clone();
	let mut t = new_test_ext(compact_code_unwrap());
	t.execute_with(|| {
		let source = TransactionSource::External;
		let extrinsic = UncheckedExtrinsic::decode(&mut &*tx0).unwrap();
		// add balance to the account
		let author = extrinsic.0.preamble.clone().to_signed().clone().unwrap().0;
		let address = Indices::lookup(author).unwrap();
		let data = pallet_balances::AccountData {
			free: ExistentialDeposit::get() * 10,
			..Default::default()
		};
		let account = frame_system::AccountInfo { providers: 1, data, ..Default::default() };
		<frame_system::Account<Runtime>>::insert(&address, account);

		// check validity
		let res = Executive::validate_transaction(
			source,
			extrinsic,
			frame_system::BlockHash::<Runtime>::get(0),
		)
		.unwrap();

		// We ignore res.priority since this number can change based on updates to weights and such.
		assert_eq!(res.requires, Vec::<TransactionTag>::new());
		assert_eq!(res.provides, vec![(address, 0).encode()]);
		assert_eq!(res.longevity, 2047);
		assert_eq!(res.propagate, true);
	});
}
