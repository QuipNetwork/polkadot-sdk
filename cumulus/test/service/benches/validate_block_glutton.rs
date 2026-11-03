// This file is part of Cumulus.

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

use codec::{Decode, Encode};
use core::time::Duration;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use cumulus_primitives_core::{relay_chain::AccountId, PersistedValidationData, ValidationParams};
use cumulus_test_client::{
	generate_extrinsic_with_pair, BlockBuilderAndSupportData, BuildBlockBuilder,
	BuildParachainBlockData, Client, ParachainBlockData, TestClientBuilder, ValidationResult,
};
use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
use cumulus_test_runtime::{Block, GluttonCall, Header, SudoCall};
use polkadot_primitives::HeadData;
use sc_client_api::UsageProvider;
use sc_consensus::{BlockImport, BlockImportParams, ForkChoiceStrategy, ImportResult, StateAction};
use sc_executor_common::wasm_runtime::{WasmModule, DEFAULT_HEAP_ALLOC_STRATEGY};
use sp_api::ProvideRuntimeApi;

use frame_system_rpc_runtime_api::AccountNonceApi;
use sp_arithmetic::{
	traits::{One, Zero},
	FixedU64,
};
use sp_consensus::BlockOrigin;
use sp_keyring::Sr25519Keyring::Alice;
use sp_runtime::traits::Header as HeaderT;

use cumulus_test_service::bench_utils as utils;

async fn import_block(
	client: &cumulus_test_client::Client,
	built: cumulus_test_runtime::Block,
	import_existing: bool,
) {
	let mut params = BlockImportParams::new(BlockOrigin::File, built.header.clone());
	params.body = Some(built.extrinsics.clone());
	params.state_action = StateAction::Execute;
	params.fork_choice = Some(ForkChoiceStrategy::LongestChain);
	params.import_existing = import_existing;
	let import_result = client.import_block(params).await;
	assert!(matches!(import_result, Ok(ImportResult::Imported(_))));
}

fn benchmark_block_validation(c: &mut Criterion) {
	sp_tracing::try_init_simple();
	let runtime = tokio::runtime::Runtime::new().expect("creating tokio runtime doesn't fail; qed");

	let endowed_accounts = vec![AccountId::from(Alice.public())];
	let mut test_client_builder = TestClientBuilder::with_default_backend();
	let genesis_init = test_client_builder.genesis_init_mut();
	*genesis_init = cumulus_test_client::GenesisParameters { endowed_accounts, wasm: None };

	let client = test_client_builder.build_with_native_executor(None).0;

	let mut group = c.benchmark_group("Block validation");
	group.sample_size(20);
	group.measurement_time(Duration::from_secs(120));

	// In the first iteration we want to initialize the glutton pallet.
	let mut is_first = true;
	for (compute_ratio, storage_ratio) in &[(One::one(), Zero::zero()), (One::one(), One::one())] {
		let parachain_block =
			set_glutton_parameters(&client, is_first, compute_ratio, storage_ratio);
		is_first = false;

		runtime.block_on(import_block(&client, parachain_block.blocks()[0].clone(), false));

		// Build benchmark block
		let parent_hash = client.usage_info().chain.best_hash;
		let parent_header = client.header(parent_hash).expect("Just fetched this hash.").unwrap();
		let validation_data = PersistedValidationData {
			relay_parent_number: 1,
			parent_head: parent_header.encode().into(),
			..Default::default()
		};
		let BlockBuilderAndSupportData { block_builder, .. } = client
			.init_block_builder_builder()
			.with_validation_data(validation_data)
			.build();
		let parachain_block = block_builder.build_parachain_block(*parent_header.state_root());

		let proof_size_in_kb = parachain_block.proof().encoded_size() as f64 / 1024f64;
		runtime.block_on(import_block(&client, parachain_block.blocks()[0].clone(), false));
		let runtime = utils::get_wasm_module();

		let sproof_builder: RelayStateSproofBuilder = Default::default();
		let (relay_parent_storage_root, _) = sproof_builder.clone().into_state_root_and_proof();
		let encoded_params = ValidationParams {
			block_data: cumulus_test_client::BlockData(parachain_block.clone().encode()),
			parent_head: HeadData(parent_header.encode()),
			relay_parent_number: 1,
			relay_parent_storage_root,
		}
		.encode();

		// This is not strictly necessary for this benchmark, but
		// let us make sure that the result of `validate_block` is what
		// we expect.
		verify_expected_result(&runtime, &encoded_params, parachain_block.blocks()[0].clone());

		group.bench_function(
			format!(
				"(compute = {:?}, storage = {:?}, proof_size = {}kb) block validation",
				compute_ratio, storage_ratio, proof_size_in_kb
			),
			|b| {
				b.iter_batched(
					|| runtime.new_instance(DEFAULT_HEAP_ALLOC_STRATEGY).unwrap(),
					|mut instance| {
						instance.call_export("validate_block", &encoded_params).unwrap();
					},
					BatchSize::SmallInput,
				)
			},
		);
	}
}

fn verify_expected_result(runtime: &Box<dyn WasmModule>, encoded_params: &[u8], block: Block) {
	let res = runtime
		.new_instance(DEFAULT_HEAP_ALLOC_STRATEGY)
		.unwrap()
		.call_export("validate_block", encoded_params)
		.expect("Call `validate_block`.");
	let validation_result =
		ValidationResult::decode(&mut &res[..]).expect("Decode `ValidationResult`.");
	let header =
		Header::decode(&mut &validation_result.head_data.0[..]).expect("Decodes `Header`.");
	assert_eq!(block.header, header);
}

fn set_glutton_parameters(
	client: &Client,
	initialize: bool,
	compute_ratio: &FixedU64,
	storage_ratio: &FixedU64,
) -> ParachainBlockData {
	let parent_hash = client.usage_info().chain.best_hash;
	let parent_header = client.header(parent_hash).expect("Just fetched this hash.").unwrap();

	let mut last_nonce = client
		.runtime_api()
		.account_nonce(parent_hash, Alice.into())
		.expect("Fetching account nonce works; qed");

	let validation_data = PersistedValidationData {
		relay_parent_number: 1,
		parent_head: parent_header.encode().into(),
		..Default::default()
	};

	let mut extrinsics = vec![];
	if initialize {
		extrinsics.push(generate_extrinsic_with_pair(
			client,
			Alice.into(),
			SudoCall::sudo {
				call: Box::new(
					GluttonCall::initialize_pallet { new_count: 5000, witness_count: None }.into(),
				),
			},
			Some(last_nonce),
		));
		last_nonce += 1;
	}

	let set_compute = generate_extrinsic_with_pair(
		client,
		Alice.into(),
		SudoCall::sudo {
			call: Box::new(GluttonCall::set_compute { compute: *compute_ratio }.into()),
		},
		Some(last_nonce),
	);
	last_nonce += 1;
	extrinsics.push(set_compute);

	let set_storage = generate_extrinsic_with_pair(
		client,
		Alice.into(),
		SudoCall::sudo {
			call: Box::new(GluttonCall::set_storage { storage: *storage_ratio }.into()),
		},
		Some(last_nonce),
	);
	extrinsics.push(set_storage);

	let BlockBuilderAndSupportData { mut block_builder, .. } = client
		.init_block_builder_builder()
		.with_validation_data(validation_data)
		.build();

	for extrinsic in extrinsics {
		block_builder.push(extrinsic).unwrap();
	}

	block_builder.build_parachain_block(*parent_header.state_root())
}

criterion_group!(benches, benchmark_block_validation);
criterion_main!(benches);
