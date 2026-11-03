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
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use cumulus_primitives_core::{
	relay_chain::AccountId, ParaId, PersistedValidationData, ValidationParams,
};
use cumulus_test_client::{
	generate_extrinsic_with_pair, BuildBlockBuilder, BuildParachainBlockData, TestClientBuilder,
	ValidationResult,
};
use cumulus_test_relay_sproof_builder::RelayStateSproofBuilder;
use cumulus_test_runtime::{BalancesCall, Block, Header, UncheckedExtrinsic};
use cumulus_test_service::bench_utils as utils;
use polkadot_primitives::HeadData;
use sc_block_builder::BlockBuilderBuilder;
use sc_client_api::UsageProvider;
use sc_executor_common::wasm_runtime::{WasmModule, DEFAULT_HEAP_ALLOC_STRATEGY};

use sp_blockchain::{ApplyExtrinsicFailed::Validity, Error::ApplyExtrinsicFailed};

use sp_core::{sr25519, Pair};

use sp_runtime::{
	traits::Header as HeaderT,
	transaction_validity::{InvalidTransaction, TransactionValidityError},
};

fn create_extrinsics(
	client: &cumulus_test_client::Client,
	src_accounts: &[sr25519::Pair],
	dst_accounts: &[sr25519::Pair],
) -> (usize, Vec<UncheckedExtrinsic>) {
	// Add as many transfer extrinsics as possible into a single block.
	let mut block_builder = BlockBuilderBuilder::new(client)
		.on_parent_block(client.chain_info().best_hash)
		.with_parent_block_number(client.chain_info().best_number)
		.build()
		.unwrap();
	let mut max_transfer_count = 0;
	let mut extrinsics = Vec::new();

	for (src, dst) in src_accounts.iter().zip(dst_accounts.iter()) {
		let extrinsic: UncheckedExtrinsic = generate_extrinsic_with_pair(
			client,
			src.clone(),
			BalancesCall::transfer_keep_alive {
				dest: AccountId::from(dst.public()).into(),
				value: 10000,
			},
			None,
		);

		match block_builder.push(extrinsic.clone()) {
			Ok(_) => {},
			Err(ApplyExtrinsicFailed(Validity(TransactionValidityError::Invalid(
				InvalidTransaction::ExhaustsResources,
			)))) => break,
			Err(error) => panic!("{}", error),
		}

		extrinsics.push(extrinsic);
		max_transfer_count += 1;
	}

	(max_transfer_count, extrinsics)
}

fn benchmark_block_validation(c: &mut Criterion) {
	sp_tracing::try_init_simple();
	// Create enough accounts to fill the block with transactions.
	// Each account should only be included in one transfer.
	let (src_accounts, dst_accounts, account_ids) = utils::create_benchmark_accounts();

	let para_id = ParaId::from(cumulus_test_runtime::PARACHAIN_ID);
	let mut test_client_builder = TestClientBuilder::with_default_backend();
	let genesis_init = test_client_builder.genesis_init_mut();
	*genesis_init =
		cumulus_test_client::GenesisParameters { endowed_accounts: account_ids, wasm: None };
	let client = test_client_builder.build_with_native_executor(None).0;

	let (max_transfer_count, extrinsics) = create_extrinsics(&client, &src_accounts, &dst_accounts);

	let parent_hash = client.usage_info().chain.best_hash;
	let parent_header = client.header(parent_hash).expect("Just fetched this hash.").unwrap();
	let validation_data = PersistedValidationData {
		relay_parent_number: 1,
		parent_head: parent_header.encode().into(),
		..Default::default()
	};

	let sproof_builder = RelayStateSproofBuilder {
		included_para_head: Some(parent_header.clone().encode().into()),
		para_id,
		..Default::default()
	};

	let cumulus_test_client::BlockBuilderAndSupportData { mut block_builder, .. } = client
		.init_block_builder_builder()
		.with_validation_data(validation_data)
		.with_relay_sproof_builder(sproof_builder.clone())
		.build();

	for extrinsic in extrinsics {
		block_builder.push(extrinsic).unwrap();
	}

	let parachain_block = block_builder.build_parachain_block(*parent_header.state_root());

	let proof_size_in_kb = parachain_block.proof().encoded_size() as f64 / 1024f64;
	let runtime = utils::get_wasm_module();

	let (relay_parent_storage_root, _) = sproof_builder.into_state_root_and_proof();
	let encoded_params = ValidationParams {
		block_data: cumulus_test_client::BlockData(parachain_block.encode()),
		parent_head: HeadData(parent_header.encode()),
		relay_parent_number: 1,
		relay_parent_storage_root,
	}
	.encode();

	// This is not strictly necessary for this benchmark, but
	// let us make sure that the result of `validate_block` is what
	// we expect.
	verify_expected_result(&runtime, &encoded_params, parachain_block.blocks()[0].clone());

	let mut group = c.benchmark_group("Block validation");
	group.sample_size(20);
	group.measurement_time(Duration::from_secs(120));
	group.throughput(Throughput::Elements(max_transfer_count as u64));

	group.bench_function(
		format!(
			"(transfers = {}, proof_size = {}kb) block validation",
			max_transfer_count, proof_size_in_kb
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

fn verify_expected_result(
	runtime: &Box<dyn WasmModule>,
	encoded_params: &[u8],
	parachain_block: Block,
) {
	let res = runtime
		.new_instance(DEFAULT_HEAP_ALLOC_STRATEGY)
		.unwrap()
		.call_export("validate_block", encoded_params)
		.expect("Call `validate_block`.");
	let validation_result =
		ValidationResult::decode(&mut &res[..]).expect("Decode `ValidationResult`.");
	let header =
		Header::decode(&mut &validation_result.head_data.0[..]).expect("Decodes `Header`.");
	assert_eq!(parachain_block.header, header);
}

criterion_group!(benches, benchmark_block_validation);
criterion_main!(benches);
