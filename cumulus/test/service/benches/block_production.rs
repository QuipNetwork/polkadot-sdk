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

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

use sc_client_api::UsageProvider;

use core::time::Duration;
use cumulus_primitives_core::ParaId;
use sc_block_builder::BlockBuilderBuilder;

use sp_keyring::Sr25519Keyring::Alice;

use cumulus_test_service::bench_utils as utils;

fn benchmark_block_production(c: &mut Criterion) {
	sp_tracing::try_init_simple();

	let runtime = tokio::runtime::Runtime::new().expect("creating tokio runtime doesn't fail; qed");
	let tokio_handle = runtime.handle();

	// Create enough accounts to fill the block with transactions.
	// Each account should only be included in one transfer.
	let (src_accounts, dst_accounts, account_ids) = utils::create_benchmark_accounts();

	let para_id = ParaId::from(100);
	let alice = runtime.block_on(
		cumulus_test_service::TestNodeBuilder::new(para_id, tokio_handle.clone(), Alice)
			// Preload all accounts with funds for the transfers
			.endowed_accounts(account_ids)
			.build(),
	);
	let client = alice.client;

	let parent_hash = client.usage_info().chain.best_hash;
	let parent_header = client.header(parent_hash).expect("Just fetched this hash.").unwrap();
	let set_validation_data_extrinsic = utils::extrinsic_set_validation_data(parent_header);

	let mut block_builder = BlockBuilderBuilder::new(&*client)
		.on_parent_block(client.chain_info().best_hash)
		.with_parent_block_number(client.chain_info().best_number)
		.build()
		.unwrap();
	block_builder.push(utils::extrinsic_set_time(&client)).unwrap();
	block_builder.push(set_validation_data_extrinsic).unwrap();
	let built_block = block_builder.build().unwrap();

	runtime.block_on(utils::import_block(&client, &built_block.block, false));

	let (max_transfer_count, extrinsics) =
		utils::create_benchmarking_transfer_extrinsics(&client, &src_accounts, &dst_accounts);

	let mut group = c.benchmark_group("Block production");

	group.sample_size(20);
	group.measurement_time(Duration::from_secs(120));
	group.throughput(Throughput::Elements(max_transfer_count as u64));

	let chain = client.chain_info();

	group.bench_function(
		format!("(proof = true, transfers = {}) block production", max_transfer_count),
		|b| {
			b.iter_batched(
				|| extrinsics.clone(),
				|extrinsics| {
					let mut block_builder = BlockBuilderBuilder::new(&*client)
						.on_parent_block(chain.best_hash)
						.with_parent_block_number(chain.best_number)
						.with_proof_recorder(Some(Default::default()))
						.build()
						.unwrap();

					for extrinsic in extrinsics {
						block_builder.push(extrinsic).unwrap();
					}
					block_builder.build().unwrap()
				},
				BatchSize::SmallInput,
			)
		},
	);

	group.bench_function(
		format!("(proof = false, transfers = {}) block production", max_transfer_count),
		|b| {
			b.iter_batched(
				|| extrinsics.clone(),
				|extrinsics| {
					let mut block_builder = BlockBuilderBuilder::new(&*client)
						.on_parent_block(chain.best_hash)
						.with_parent_block_number(chain.best_number)
						.build()
						.unwrap();

					for extrinsic in extrinsics {
						block_builder.push(extrinsic).unwrap();
					}
					block_builder.build().unwrap()
				},
				BatchSize::SmallInput,
			)
		},
	);
}

criterion_group!(benches, benchmark_block_production);
criterion_main!(benches);
