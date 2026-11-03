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

use std::{
	panic::UnwindSafe,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
};

use sc_block_builder::BlockBuilderBuilder;
use sp_api::{ApiExt, Core, ProofRecorder, ProvideRuntimeApi};
use sp_externalities::{decl_extension, TransactionType};
use sp_runtime::{
	traits::{HashingFor, Header as HeaderT},
	TransactionOutcome,
};
use sp_state_machine::{
	create_proof_check_backend, execution_proof_check_on_trie_backend, OverlayedChanges,
};

use substrate_test_runtime_client::{
	prelude::*,
	runtime::{Block, Header, TestAPI, Transfer},
	DefaultTestClientBuilderExt, TestClient, TestClientBuilder,
};

use codec::Encode;
use sp_consensus::SelectChain;
use substrate_test_runtime_client::sc_executor::WasmExecutor;

#[test]
fn calling_runtime_function() {
	let client = TestClientBuilder::new().build();
	let runtime_api = client.runtime_api();
	let best_hash = client.chain_info().best_hash;

	assert_eq!(runtime_api.benchmark_add_one(best_hash, &1).unwrap(), 2);
}

#[test]
fn calling_native_runtime_signature_changed_function() {
	let client = TestClientBuilder::new().build();
	let runtime_api = client.runtime_api();
	let best_hash = client.chain_info().best_hash;

	assert_eq!(runtime_api.function_signature_changed(best_hash).unwrap(), 1);
}

#[test]
fn use_trie_function() {
	let client = TestClientBuilder::new().build();
	let runtime_api = client.runtime_api();
	let best_hash = client.chain_info().best_hash;
	assert_eq!(runtime_api.use_trie(best_hash).unwrap(), 2);
}

#[test]
fn initialize_block_works() {
	let client = TestClientBuilder::new().build();
	let runtime_api = client.runtime_api();
	let best_hash = client.chain_info().best_hash;
	runtime_api
		.initialize_block(
			best_hash,
			&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				Default::default(),
			),
		)
		.unwrap();
	assert_eq!(runtime_api.get_block_number(best_hash).unwrap(), 1);
}

#[test]
fn record_proof_works() {
	let (client, longest_chain) = TestClientBuilder::new().build_with_longest_chain();

	let storage_root =
		*futures::executor::block_on(longest_chain.best_chain()).unwrap().state_root();

	let runtime_code = sp_core::traits::RuntimeCode {
		code_fetcher: &sp_core::traits::WrappedRuntimeCode(
			client.code_at(client.chain_info().best_hash).unwrap().into(),
		),
		hash: vec![1],
		heap_pages: None,
	};

	let transaction = Transfer {
		amount: 1000,
		nonce: 0,
		from: Sr25519Keyring::Alice.into(),
		to: Sr25519Keyring::Bob.into(),
	}
	.into_unchecked_extrinsic();

	let storage_proof_recorder = ProofRecorder::<Block>::default();

	// Build the block and record proof
	let mut builder = BlockBuilderBuilder::new(&client)
		.on_parent_block(client.chain_info().best_hash)
		.with_parent_block_number(client.chain_info().best_number)
		.with_proof_recorder(storage_proof_recorder.clone())
		.build()
		.unwrap();
	builder.push(transaction.clone()).unwrap();

	let (block, _) = builder.build().expect("Bake block").into_inner();

	let proof = storage_proof_recorder.drain_storage_proof();

	let backend = create_proof_check_backend::<HashingFor<Block>>(storage_root, proof)
		.expect("Creates proof backend.");

	// Use the proof backend to execute `execute_block`.
	let mut overlay = Default::default();
	let executor: WasmExecutor = WasmExecutor::builder().build();
	execution_proof_check_on_trie_backend(
		&backend,
		&mut overlay,
		&executor,
		"Core_execute_block",
		&block.encode(),
		&runtime_code,
	)
	.expect("Executes block while using the proof backend");
}

#[test]
fn call_runtime_api_with_multiple_arguments() {
	let client = TestClientBuilder::new().build();

	let data = vec![1, 2, 4, 5, 6, 7, 8, 8, 10, 12];
	let best_hash = client.chain_info().best_hash;
	client
		.runtime_api()
		.test_multiple_arguments(best_hash, data.clone(), data.clone(), data.len() as u32)
		.unwrap();
}

#[test]
fn disable_logging_works() {
	if std::env::var("RUN_TEST").is_ok() {
		sp_tracing::try_init_simple();

		let mut builder = TestClientBuilder::new();
		builder.genesis_init_mut().set_wasm_code(
			substrate_test_runtime_client::runtime::wasm_binary_logging_disabled_unwrap().to_vec(),
		);

		let client = builder.build();
		let runtime_api = client.runtime_api();
		runtime_api
			.do_trace_log(client.chain_info().genesis_hash)
			.expect("Logging should not fail");
		log::error!("Logging from native works");
	} else {
		let executable = std::env::current_exe().unwrap();
		let output = std::process::Command::new(executable)
			.env("RUN_TEST", "1")
			.env("RUST_LOG", "info")
			.args(&["--nocapture", "disable_logging_works"])
			.output()
			.unwrap();

		let output = dbg!(String::from_utf8(output.stderr).unwrap());
		assert!(!output.contains("Hey I'm runtime"));
		assert!(output.contains("Logging from native works"));
	}
}

// Certain logic like the transaction handling is not unwind safe.
//
// Ensure that the type is not unwind safe!
static_assertions::assert_not_impl_any!(<TestClient as ProvideRuntimeApi<_>>::Api: UnwindSafe);

#[derive(Default)]
struct TransactionTesterInner {
	started: AtomicUsize,
	committed: AtomicUsize,
	rolled_back: AtomicUsize,
}

decl_extension! {
	struct TransactionTester(Arc<TransactionTesterInner>);

	impl TransactionTester {
		fn start_transaction(&mut self, ty: TransactionType) {
			assert_eq!(ty, TransactionType::Host);
			self.0.started.fetch_add(1, Ordering::Relaxed);
		}

		fn commit_transaction(&mut self, ty: TransactionType) {
			assert_eq!(ty, TransactionType::Host);
			self.0.committed.fetch_add(1, Ordering::Relaxed);
		}

		fn rollback_transaction(&mut self, ty: TransactionType) {
			assert_eq!(ty, TransactionType::Host);
			self.0.rolled_back.fetch_add(1, Ordering::Relaxed);
		}
	}
}

#[test]
fn ensure_transactional_works() {
	const KEY: &[u8] = b"test";

	let client = TestClientBuilder::new().build();
	let best_hash = client.chain_info().best_hash;
	let transaction_tester = Arc::new(TransactionTesterInner::default());

	let mut runtime_api = client.runtime_api();
	runtime_api.register_extension(TransactionTester(transaction_tester.clone()));
	runtime_api.execute_in_transaction(|api| {
		api.write_key_value(best_hash, KEY.to_vec(), vec![1, 2, 3], false).unwrap();

		api.execute_in_transaction(|api| {
			api.write_key_value(best_hash, KEY.to_vec(), vec![1, 2, 3, 4], false).unwrap();

			TransactionOutcome::Commit(())
		});

		TransactionOutcome::Commit(())
	});

	let changes = runtime_api
		.into_storage_changes(&client.state_at(best_hash).unwrap(), best_hash)
		.unwrap();
	assert_eq!(changes.main_storage_changes[0].1, Some(vec![1, 2, 3, 4]));

	let mut runtime_api = client.runtime_api();
	runtime_api.register_extension(TransactionTester(transaction_tester.clone()));
	runtime_api.execute_in_transaction(|api| {
		assert!(api.write_key_value(best_hash, KEY.to_vec(), vec![1, 2, 3], true).is_err());

		TransactionOutcome::Commit(())
	});

	let changes = runtime_api
		.into_storage_changes(&client.state_at(best_hash).unwrap(), best_hash)
		.unwrap();
	assert_eq!(changes.main_storage_changes[0].1, Some(vec![1, 2, 3]));

	let mut runtime_api = client.runtime_api();
	runtime_api.register_extension(TransactionTester(transaction_tester.clone()));
	runtime_api.execute_in_transaction(|api| {
		assert!(api.write_key_value(best_hash, KEY.to_vec(), vec![1, 2], true).is_err());

		TransactionOutcome::Rollback(())
	});

	let changes = runtime_api
		.into_storage_changes(&client.state_at(best_hash).unwrap(), best_hash)
		.unwrap();
	assert!(changes.main_storage_changes.is_empty());

	assert_eq!(transaction_tester.started.load(Ordering::Relaxed), 4);
	assert_eq!(transaction_tester.committed.load(Ordering::Relaxed), 3);
	assert_eq!(transaction_tester.rolled_back.load(Ordering::Relaxed), 1);
}

#[test]
fn set_overlayed_changes_is_observed_by_typed_call() {
	let system_number_key: Vec<u8> = sp_crypto_hashing::twox_128(b"System")
		.iter()
		.chain(sp_crypto_hashing::twox_128(b"Number").iter())
		.copied()
		.collect();

	let client = TestClientBuilder::new().build();
	let mut runtime_api = client.runtime_api();
	let best_hash = client.chain_info().best_hash;

	assert_eq!(runtime_api.get_block_number(best_hash).unwrap(), 0);

	let mut overlayed: OverlayedChanges<HashingFor<Block>> = OverlayedChanges::default();
	overlayed.set_storage(system_number_key, Some(42u64.encode()));
	runtime_api.set_overlayed_changes(overlayed);

	assert_eq!(runtime_api.get_block_number(best_hash).unwrap(), 42);
}
