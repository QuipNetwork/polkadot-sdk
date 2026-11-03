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

use criterion::{criterion_group, criterion_main, BatchSize, Criterion};

use sp_arithmetic::{
	traits::{One, Zero},
	FixedPointNumber,
};

use core::time::Duration;
use cumulus_primitives_core::ParaId;

use sc_block_builder::BlockBuilderBuilder;

use sp_keyring::Sr25519Keyring::Alice;

use cumulus_test_service::bench_utils as utils;

fn benchmark_block_production_compute(c: &mut Criterion) {
	sp_tracing::try_init_simple();

	let runtime = tokio::runtime::Runtime::new().expect("creating tokio runtime doesn't fail; qed");
	let tokio_handle = runtime.handle();

	let para_id = ParaId::from(100);
	let alice = runtime.block_on(
		cumulus_test_service::TestNodeBuilder::new(para_id, tokio_handle.clone(), Alice).build(),
	);
	let client = alice.client;

	let mut group = c.benchmark_group("Block production");

	group.sample_size(20);
	group.measurement_time(Duration::from_secs(120));

	let mut initialize_glutton_pallet = true;
	for (compute_ratio, storage_ratio) in &[(One::one(), Zero::zero()), (One::one(), One::one())] {
		let block = utils::set_glutton_parameters(
			&client,
			initialize_glutton_pallet,
			compute_ratio,
			storage_ratio,
		);
		runtime.block_on(utils::import_block(&client, &block, false));
		initialize_glutton_pallet = false;

		let best_hash = client.chain_info().best_hash;
		let best_number = client.chain_info().best_number;
		let parent_header = client.header(best_hash).expect("Just fetched this hash.").unwrap();
		let set_validation_data_extrinsic = utils::extrinsic_set_validation_data(parent_header);
		let set_time_extrinsic = utils::extrinsic_set_time(&client);

		group.bench_function(
			format!(
				"(compute = {:?} %, storage = {:?} %) block import",
				compute_ratio.saturating_mul_int(100),
				storage_ratio.saturating_mul_int(100)
			),
			|b| {
				b.iter_batched(
					|| (set_validation_data_extrinsic.clone(), set_time_extrinsic.clone()),
					|(validation_data, time)| {
						let mut block_builder = BlockBuilderBuilder::new(&*client)
							.on_parent_block(best_hash)
							.with_parent_block_number(best_number)
							.with_proof_recorder(Some(Default::default()))
							.build()
							.unwrap();
						block_builder.push(validation_data).unwrap();
						block_builder.push(time).unwrap();
						block_builder.build().unwrap()
					},
					BatchSize::SmallInput,
				)
			},
		);

		group.bench_function(
			format!(
				"(compute = {:?} %, storage = {:?} %) block import",
				compute_ratio.saturating_mul_int(100),
				storage_ratio.saturating_mul_int(100)
			),
			|b| {
				b.iter_batched(
					|| (set_validation_data_extrinsic.clone(), set_time_extrinsic.clone()),
					|(validation_data, time)| {
						let mut block_builder = BlockBuilderBuilder::new(&*client)
							.on_parent_block(best_hash)
							.with_parent_block_number(best_number)
							.build()
							.unwrap();
						block_builder.push(validation_data).unwrap();
						block_builder.push(time).unwrap();
						block_builder.build().unwrap()
					},
					BatchSize::SmallInput,
				)
			},
		);
	}
}

criterion_group!(benches, benchmark_block_production_compute);
criterion_main!(benches);
