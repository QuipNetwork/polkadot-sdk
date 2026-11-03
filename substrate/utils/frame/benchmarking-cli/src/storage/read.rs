// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use codec::Encode;
use frame_storage_access_test_runtime::StorageAccessParams;
use log::{debug, info};
use rand::prelude::*;
use sc_cli::{Error, Result};
use sc_client_api::{Backend as ClientBackend, StorageProvider, UsageProvider};
use sp_api::CallApiAt;
use sp_runtime::traits::{Block as BlockT, HashingFor, Header as HeaderT};
use sp_state_machine::{backend::AsTrieBackend, Backend};
use sp_storage::ChildInfo;
use sp_trie::StorageProof;
use std::{fmt::Debug, sync::Arc, time::Instant};

use super::{
	cmd::StorageCmd,
	get_wasm_module,
	keys_selection::{select_entries, EmptyStorage as SelectEntriesEmptyStorage},
	MAX_BATCH_SIZE_FOR_BLOCK_VALIDATION,
};
use crate::shared::BenchRecord;

impl StorageCmd {
	/// Benchmarks the time it takes to read a single Storage item.
	/// Uses the latest state that is available for the given client.
	pub(crate) fn bench_read<B, BA, C>(
		&self,
		client: Arc<C>,
		_shared_trie_cache: Option<sp_trie::cache::SharedTrieCache<HashingFor<B>>>,
	) -> Result<BenchRecord>
	where
		C: UsageProvider<B> + StorageProvider<B, BA> + CallApiAt<B>,
		B: BlockT + Debug,
		BA: ClientBackend<B>,
		<<B as BlockT>::Header as HeaderT>::Number: From<u32>,
	{
		if self.params.is_validate_block_mode() && self.params.disable_pov_recorder {
			return Err("PoV recorder must be activated to provide a storage proof for block validation at runtime. Remove `--disable-pov-recorder` from the command line.".into());
		}
		if self.params.is_validate_block_mode() &&
			self.params.batch_size > MAX_BATCH_SIZE_FOR_BLOCK_VALIDATION
		{
			return Err(format!("Batch size is too large. This may cause problems with runtime memory allocation. Better set `--batch-size {}` or less.", MAX_BATCH_SIZE_FOR_BLOCK_VALIDATION).into());
		}

		let mut record = BenchRecord::default();
		let best_hash = client.usage_info().chain.best_hash;

		info!("Preparing keys from block {}", best_hash);
		let (keys, mut rng) = select_entries(
			self.params.keys_limit,
			self.params.random_seed,
			|first_key_ref| {
				let fk = first_key_ref.map(|b| sp_storage::StorageKey(b.to_vec()));
				Ok(client.storage_keys(best_hash, None, fk.as_ref())?)
			},
			|| Ok(client.storage_keys(best_hash, None, None)?),
			|k: &sp_storage::StorageKey| k.0.as_slice(),
		)?;

		let mut child_nodes = Vec::new();
		// Interesting part here:
		// Read all the keys in the database and measure the time it takes to access each.
		info!("Reading {} keys", keys.len());

		// Read using the same TrieBackend and recorder for up to `batch_size` keys.
		// This would allow us to measure the amortized cost of reading a key.
		let state = client
			.state_at(best_hash)
			.map_err(|_err| Error::Input("State not found".into()))?;
		// We reassign the backend and recorder for every batch size.
		// Using a new recorder for every read vs using the same for the entire batch
		// produces significant different results. Since in the real use case we use a
		// single recorder per block, simulate the same behavior by creating a new
		// recorder every batch size, so that the amortized cost of reading a key is
		// measured in conditions closer to the real world.
		let (mut backend, mut recorder) = self.create_backend::<B, C>(&state);

		let mut read_in_batch = 0;
		let mut on_validation_batch = vec![];
		let mut on_validation_size = 0;

		let last_key = keys.last().expect("Checked above to be non-empty");
		for key in keys.as_slice() {
			match (self.params.include_child_trees, self.is_child_key(key.clone().0)) {
				(true, Some(info)) => {
					// child tree key: sample with select_entries when child_keys_limit is set
					match select_entries(
						self.params.child_keys_limit,
						self.params.random_seed,
						|first_key_ref| {
							let fk = first_key_ref.map(|b| sp_storage::StorageKey(b.to_vec()));
							Ok(client
								.child_storage_keys(best_hash, info.clone(), None, fk.as_ref())?
								.map(|ck| (ck, info.clone())))
						},
						|| {
							Ok(client
								.child_storage_keys(best_hash, info.clone(), None, None)?
								.map(|ck| (ck, info.clone())))
						},
						|(k, _): &(sp_storage::StorageKey, ChildInfo)| k.0.as_slice(),
					) {
						Ok((entries, _)) => child_nodes.extend(entries),
						Err(SelectEntriesEmptyStorage::Input(_)) => {},
						Err(e) => return Err(e),
					}
				},
				_ => {
					// regular key
					on_validation_batch.push((key.0.clone(), None));
					let start = Instant::now();
					let v = backend
						.storage(key.0.as_ref())
						.expect("Checked above to exist")
						.ok_or("Value unexpectedly empty")?;
					on_validation_size += v.len();
					if self.params.is_import_block_mode() {
						record.append(v.len(), start.elapsed())?;
					}
				},
			}
			read_in_batch += 1;
			let is_batch_full = read_in_batch >= self.params.batch_size || key == last_key;

			// Read keys on block validation
			if is_batch_full && self.params.is_validate_block_mode() {
				let root = backend.root();
				let storage_proof = recorder
					.clone()
					.map(|r| r.drain_storage_proof())
					.expect("Storage proof must exist for block validation");
				let elapsed = measure_block_validation::<B>(
					*root,
					storage_proof,
					on_validation_batch.clone(),
					self.params.validate_block_rounds,
				);
				record.append(on_validation_size / on_validation_batch.len(), elapsed)?;

				on_validation_batch = vec![];
				on_validation_size = 0;
			}

			// Reload recorder
			if is_batch_full {
				(backend, recorder) = self.create_backend::<B, C>(&state);
				read_in_batch = 0;
			}
		}

		if self.params.include_child_trees && !child_nodes.is_empty() {
			child_nodes.shuffle(&mut rng);

			info!("Reading {} child keys", child_nodes.len());
			let (last_child_key, last_child_info) =
				child_nodes.last().expect("Checked above to be non-empty");
			for (key, info) in child_nodes.as_slice() {
				on_validation_batch.push((key.0.clone(), Some(info.clone())));
				let start = Instant::now();
				let v = backend
					.child_storage(info, key.0.as_ref())
					.expect("Checked above to exist")
					.ok_or("Value unexpectedly empty")?;
				on_validation_size += v.len();
				if self.params.is_import_block_mode() {
					record.append(v.len(), start.elapsed())?;
				}
				read_in_batch += 1;
				let is_batch_full = read_in_batch >= self.params.batch_size ||
					(last_child_key == key && last_child_info == info);

				// Read child keys on block validation
				if is_batch_full && self.params.is_validate_block_mode() {
					let root = backend.root();
					let storage_proof = recorder
						.clone()
						.map(|r| r.drain_storage_proof())
						.expect("Storage proof must exist for block validation");
					let elapsed = measure_block_validation::<B>(
						*root,
						storage_proof,
						on_validation_batch.clone(),
						self.params.validate_block_rounds,
					);
					record.append(on_validation_size / on_validation_batch.len(), elapsed)?;

					on_validation_batch = vec![];
					on_validation_size = 0;
				}

				// Reload recorder
				if is_batch_full {
					(backend, recorder) = self.create_backend::<B, C>(&state);
					read_in_batch = 0;
				}
			}
		}

		Ok(record)
	}

	fn create_backend<'a, B, C>(
		&self,
		state: &'a C::StateBackend,
	) -> (
		sp_state_machine::TrieBackend<
			&'a <C::StateBackend as AsTrieBackend<HashingFor<B>>>::TrieBackendStorage,
			HashingFor<B>,
			&'a sp_trie::cache::LocalTrieCache<HashingFor<B>>,
		>,
		Option<sp_trie::recorder::Recorder<HashingFor<B>>>,
	)
	where
		C: CallApiAt<B>,
		B: BlockT + Debug,
	{
		let recorder = (!self.params.disable_pov_recorder).then(|| Default::default());
		let backend = sp_state_machine::TrieBackendBuilder::wrap(state.as_trie_backend())
			.with_optional_recorder(recorder.clone())
			.build();

		(backend, recorder)
	}
}

fn measure_block_validation<B: BlockT + Debug>(
	root: B::Hash,
	storage_proof: StorageProof,
	on_validation_batch: Vec<(Vec<u8>, Option<ChildInfo>)>,
	rounds: u32,
) -> std::time::Duration {
	debug!(
		"POV: len {:?} {:?}",
		storage_proof.len(),
		storage_proof.clone().encoded_compact_size::<HashingFor<B>>(root)
	);
	let batch_size = on_validation_batch.len();
	let wasm_module = get_wasm_module();
	let mut instance = wasm_module
		.new_instance(sc_executor_common::wasm_runtime::DEFAULT_HEAP_ALLOC_STRATEGY)
		.expect("Failed to create wasm instance");
	let params = StorageAccessParams::<B>::new_read(root, storage_proof, on_validation_batch);
	let dry_run_encoded = params.as_dry_run().encode();
	let encoded = params.encode();

	let mut durations_in_nanos = Vec::new();

	for i in 1..=rounds {
		info!("validate_block with {} keys, round {}/{}", batch_size, i, rounds);

		// Dry run to get the time it takes without storage access
		let dry_run_start = Instant::now();
		instance
			.call_export("validate_block", &dry_run_encoded)
			.expect("Failed to call validate_block");
		let dry_run_elapsed = dry_run_start.elapsed();
		debug!("validate_block dry-run time {:?}", dry_run_elapsed);

		let start = Instant::now();
		instance
			.call_export("validate_block", &encoded)
			.expect("Failed to call validate_block");
		let elapsed = start.elapsed();
		debug!("validate_block time {:?}", elapsed);

		durations_in_nanos
			.push(elapsed.saturating_sub(dry_run_elapsed).as_nanos() as u64 / batch_size as u64);
	}

	std::time::Duration::from_nanos(
		durations_in_nanos.iter().sum::<u64>() / durations_in_nanos.len() as u64,
	)
}
