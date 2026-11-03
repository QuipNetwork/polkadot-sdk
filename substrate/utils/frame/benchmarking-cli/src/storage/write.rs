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
use log::{debug, info, trace, warn};
use rand::prelude::*;
use sc_cli::Result;
use sc_client_api::{Backend as ClientBackend, StorageProvider, UsageProvider};
use sc_client_db::{DbHash, DbState, DbStateBuilder};
use sp_blockchain::HeaderBackend;
use sp_database::{ColumnId, Transaction};
use sp_runtime::traits::{Block as BlockT, HashingFor, Header as HeaderT};
use sp_state_machine::Backend as StateBackend;
use sp_storage::{ChildInfo, StateVersion};
use sp_trie::{recorder::Recorder, PrefixedMemoryDB};
use std::{
	fmt::Debug,
	sync::Arc,
	time::{Duration, Instant},
};

use super::{
	cmd::StorageCmd,
	get_wasm_module,
	keys_selection::{select_entries, EmptyStorage as SelectEntriesEmptyStorage},
	MAX_BATCH_SIZE_FOR_BLOCK_VALIDATION,
};
use crate::shared::BenchRecord;

impl StorageCmd {
	/// Benchmarks the time it takes to write a single Storage item.
	///
	/// Uses the latest state that is available for the given client.
	///
	/// Unlike reading benchmark, where we read every single key, here we write a batch of keys in
	/// one time. So writing a remaining keys with the size much smaller than batch size can
	/// dramatically distort the results. To avoid this, we skip the remaining keys.
	pub(crate) fn bench_write<Block, BA, H, C>(
		&self,
		client: Arc<C>,
		(db, state_col): (Arc<dyn sp_database::Database<DbHash>>, ColumnId),
		storage: Arc<dyn sp_state_machine::Storage<HashingFor<Block>>>,
		shared_trie_cache: Option<sp_trie::cache::SharedTrieCache<HashingFor<Block>>>,
	) -> Result<BenchRecord>
	where
		Block: BlockT<Header = H, Hash = DbHash> + Debug,
		H: HeaderT<Hash = DbHash>,
		BA: ClientBackend<Block>,
		C: UsageProvider<Block> + HeaderBackend<Block> + StorageProvider<Block, BA>,
	{
		if self.params.is_validate_block_mode() && self.params.disable_pov_recorder {
			return Err("PoV recorder must be activated to provide a storage proof for block validation at runtime. Remove `--disable-pov-recorder`.".into());
		}
		if self.params.is_validate_block_mode() &&
			self.params.batch_size > MAX_BATCH_SIZE_FOR_BLOCK_VALIDATION
		{
			return Err(format!("Batch size is too large. This may cause problems with runtime memory allocation. Better set `--batch-size {}` or less.", MAX_BATCH_SIZE_FOR_BLOCK_VALIDATION).into());
		}

		// Store the time that it took to write each value.
		let mut record = BenchRecord::default();

		let best_hash = client.usage_info().chain.best_hash;
		let header = client.header(best_hash)?.ok_or("Header not found")?;
		let original_root = *header.state_root();

		let (trie, _) = self.create_trie_backend::<Block, H>(
			original_root,
			&storage,
			shared_trie_cache.as_ref(),
		);

		info!("Preparing keys from block {}", best_hash);
		let (kvs, mut rng) = select_entries(
			self.params.keys_limit,
			self.params.random_seed,
			|first_key_ref| {
				let mut iter_args = sp_state_machine::IterArgs::default();
				iter_args.start_at = first_key_ref;
				Ok(trie.pairs(iter_args)?)
			},
			|| Ok(trie.pairs(Default::default())?),
			|r| r.as_ref().map(|(k, _)| k.as_slice()).unwrap_or(&[]),
		)?;

		info!("Writing {} keys in batches of {}", kvs.len(), self.params.batch_size);
		let remainder = kvs.len() % self.params.batch_size;
		if self.params.is_validate_block_mode() && remainder != 0 {
			info!("Remaining `{remainder}` keys will be skipped");
		}

		let mut child_nodes = Vec::new();
		let mut batched_keys = Vec::new();
		// Generate all random values first; Make sure there are no collisions with existing
		// db entries, so we can rollback all additions without corrupting existing entries.

		for key_value in kvs {
			let (k, original_v) = key_value?;
			match (self.params.include_child_trees, self.is_child_key(k.to_vec())) {
				(true, Some(info)) => {
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
						|(key, _): &(sp_storage::StorageKey, ChildInfo)| key.0.as_slice(),
					) {
						Ok((entries, _)) => child_nodes.push((
							entries.into_iter().map(|(ck, _)| ck).collect::<Vec<_>>(),
							info.clone(),
						)),
						Err(SelectEntriesEmptyStorage::Input(_)) => {},
						Err(e) => return Err(e),
					}
				},
				_ => {
					// regular key
					let mut new_v = vec![0; original_v.len()];

					loop {
						// Create a random value to overwrite with.
						// NOTE: We use a possibly higher entropy than the original value,
						// could be improved but acts as an over-estimation which is fine for now.
						rng.fill_bytes(&mut new_v[..]);
						if check_new_value::<Block>(
							db.clone(),
							&trie,
							&k.to_vec(),
							&new_v,
							self.state_version(),
							state_col,
							None,
						) {
							break;
						}
					}

					batched_keys.push((k.to_vec(), new_v.to_vec()));
					if batched_keys.len() < self.params.batch_size {
						continue;
					}

					// Write each value in one commit.
					let (size, duration) = if self.params.is_validate_block_mode() {
						self.measure_per_key_amortised_validate_block_write_cost::<Block, H>(
							original_root,
							&storage,
							shared_trie_cache.as_ref(),
							batched_keys.clone(),
							None,
						)?
					} else {
						self.measure_per_key_amortised_import_block_write_cost::<Block, H>(
							original_root,
							&storage,
							shared_trie_cache.as_ref(),
							db.clone(),
							batched_keys.clone(),
							self.state_version(),
							state_col,
							None,
						)?
					};
					record.append(size, duration)?;
					batched_keys.clear();
				},
			}
		}

		if self.params.include_child_trees && !child_nodes.is_empty() {
			info!("Writing {} child keys", child_nodes.iter().map(|(c, _)| c.len()).sum::<usize>());
			for (mut child_keys, info) in child_nodes {
				if child_keys.len() < self.params.batch_size {
					warn!(
						"{} child keys will be skipped because it's less than batch size",
						child_keys.len()
					);
					continue;
				}

				child_keys.shuffle(&mut rng);

				for key in child_keys {
					if let Some(original_v) = client
						.child_storage(best_hash, &info, &key)
						.expect("Checked above to exist")
					{
						let mut new_v = vec![0; original_v.0.len()];

						loop {
							rng.fill_bytes(&mut new_v[..]);
							if check_new_value::<Block>(
								db.clone(),
								&trie,
								&key.0,
								&new_v,
								self.state_version(),
								state_col,
								Some(&info),
							) {
								break;
							}
						}
						batched_keys.push((key.0, new_v.to_vec()));
						if batched_keys.len() < self.params.batch_size {
							continue;
						}

						let (size, duration) = if self.params.is_validate_block_mode() {
							self.measure_per_key_amortised_validate_block_write_cost::<Block, H>(
								original_root,
								&storage,
								shared_trie_cache.as_ref(),
								batched_keys.clone(),
								None,
							)?
						} else {
							self.measure_per_key_amortised_import_block_write_cost::<Block, H>(
								original_root,
								&storage,
								shared_trie_cache.as_ref(),
								db.clone(),
								batched_keys.clone(),
								self.state_version(),
								state_col,
								Some(&info),
							)?
						};
						record.append(size, duration)?;
						batched_keys.clear();
					}
				}
			}
		}

		Ok(record)
	}

	fn create_trie_backend<Block, H>(
		&self,
		original_root: Block::Hash,
		storage: &Arc<dyn sp_state_machine::Storage<HashingFor<Block>>>,
		shared_trie_cache: Option<&sp_trie::cache::SharedTrieCache<HashingFor<Block>>>,
	) -> (DbState<HashingFor<Block>>, Option<Recorder<HashingFor<Block>>>)
	where
		Block: BlockT<Header = H, Hash = DbHash> + Debug,
		H: HeaderT<Hash = DbHash>,
	{
		let recorder = (!self.params.disable_pov_recorder).then(|| Default::default());
		let trie = DbStateBuilder::<HashingFor<Block>>::new(storage.clone(), original_root)
			.with_optional_cache(shared_trie_cache.map(|c| c.local_cache_trusted()))
			.with_optional_recorder(recorder.clone())
			.build();

		(trie, recorder)
	}

	/// Measures write benchmark
	/// if `child_info` exist then it means this is a child tree key
	fn measure_per_key_amortised_import_block_write_cost<Block, H>(
		&self,
		original_root: Block::Hash,
		storage: &Arc<dyn sp_state_machine::Storage<HashingFor<Block>>>,
		shared_trie_cache: Option<&sp_trie::cache::SharedTrieCache<HashingFor<Block>>>,
		db: Arc<dyn sp_database::Database<DbHash>>,
		changes: Vec<(Vec<u8>, Vec<u8>)>,
		version: StateVersion,
		col: ColumnId,
		child_info: Option<&ChildInfo>,
	) -> Result<(usize, Duration)>
	where
		Block: BlockT<Header = H, Hash = DbHash> + Debug,
		H: HeaderT<Hash = DbHash>,
	{
		let batch_size = changes.len();
		let average_len = changes.iter().map(|(_, v)| v.len()).sum::<usize>() / batch_size;
		// For every batched write use a different trie instance and recorder, so we
		// don't benefit from past runs.
		let (trie, _recorder) =
			self.create_trie_backend::<Block, H>(original_root, storage, shared_trie_cache);

		let start = Instant::now();
		// Create a TX that will modify the Trie in the DB and
		// calculate the root hash of the Trie after the modification.
		let replace = changes
			.iter()
			.map(|(key, new_v)| (key.as_ref(), Some(new_v.as_ref())))
			.collect::<Vec<_>>();
		let stx = match child_info {
			Some(info) => trie.child_storage_root(info, replace.iter().cloned(), version).2,
			None => trie.storage_root(replace.iter().cloned(), version).1,
		};
		// Only the keep the insertions, since we do not want to benchmark pruning.
		let tx = convert_tx::<Block>(db.clone(), stx.clone(), false, col);
		db.commit(tx).map_err(|e| format!("Writing to the Database: {}", e))?;
		let result = (average_len, start.elapsed() / batch_size as u32);

		// Now undo the changes by removing what was added.
		let tx = convert_tx::<Block>(db.clone(), stx.clone(), true, col);
		db.commit(tx).map_err(|e| format!("Writing to the Database: {}", e))?;

		Ok(result)
	}

	/// Measures write benchmark on block validation
	/// if `child_info` exist then it means this is a child tree key
	fn measure_per_key_amortised_validate_block_write_cost<Block, H>(
		&self,
		original_root: Block::Hash,
		storage: &Arc<dyn sp_state_machine::Storage<HashingFor<Block>>>,
		shared_trie_cache: Option<&sp_trie::cache::SharedTrieCache<HashingFor<Block>>>,
		changes: Vec<(Vec<u8>, Vec<u8>)>,
		maybe_child_info: Option<&ChildInfo>,
	) -> Result<(usize, Duration)>
	where
		Block: BlockT<Header = H, Hash = DbHash> + Debug,
		H: HeaderT<Hash = DbHash>,
	{
		let batch_size = changes.len();
		let average_len = changes.iter().map(|(_, v)| v.len()).sum::<usize>() / batch_size;
		let (trie, recorder) =
			self.create_trie_backend::<Block, H>(original_root, storage, shared_trie_cache);
		for (key, _) in changes.iter() {
			let _v = trie
				.storage(key)
				.expect("Checked above to exist")
				.ok_or("Value unexpectedly empty")?;
		}
		let storage_proof = recorder
			.map(|r| r.drain_storage_proof())
			.expect("Storage proof must exist for block validation");
		let root = trie.root();
		debug!(
			"POV: len {:?} {:?}",
			storage_proof.len(),
			storage_proof.clone().encoded_compact_size::<HashingFor<Block>>(*root)
		);
		let params = StorageAccessParams::<Block>::new_write(
			*root,
			storage_proof,
			(changes, maybe_child_info.cloned()),
		);

		let mut durations_in_nanos = Vec::new();
		let wasm_module = get_wasm_module();
		let mut instance = wasm_module
			.new_instance(sc_executor_common::wasm_runtime::DEFAULT_HEAP_ALLOC_STRATEGY)
			.expect("Failed to create wasm instance");
		let dry_run_encoded = params.as_dry_run().encode();
		let encoded = params.encode();

		for i in 1..=self.params.validate_block_rounds {
			info!(
				"validate_block with {} keys, round {}/{}",
				batch_size, i, self.params.validate_block_rounds
			);

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

			durations_in_nanos.push(
				elapsed.saturating_sub(dry_run_elapsed).as_nanos() as u64 / batch_size as u64,
			);
		}

		let result = (
			average_len,
			std::time::Duration::from_nanos(
				durations_in_nanos.iter().sum::<u64>() / durations_in_nanos.len() as u64,
			),
		);

		Ok(result)
	}
}

/// Converts a Trie transaction into a DB transaction.
/// Removals are ignored and will not be included in the final tx.
/// `invert_inserts` replaces all inserts with removals.
fn convert_tx<B: BlockT>(
	db: Arc<dyn sp_database::Database<DbHash>>,
	mut tx: PrefixedMemoryDB<HashingFor<B>>,
	invert_inserts: bool,
	col: ColumnId,
) -> Transaction<DbHash> {
	let mut ret = Transaction::<DbHash>::default();

	for (mut k, (v, rc)) in tx.drain().into_iter() {
		if rc > 0 {
			db.sanitize_key(&mut k);
			if invert_inserts {
				ret.remove(col, &k);
			} else {
				ret.set(col, &k, &v);
			}
		}
		// < 0 means removal - ignored.
		// 0 means no modification.
	}
	ret
}

/// Checks if a new value causes any collision in tree updates
/// returns true if there is no collision
/// if `child_info` exist then it means this is a child tree key
fn check_new_value<Block: BlockT>(
	db: Arc<dyn sp_database::Database<DbHash>>,
	trie: &DbState<HashingFor<Block>>,
	key: &Vec<u8>,
	new_v: &Vec<u8>,
	version: StateVersion,
	col: ColumnId,
	child_info: Option<&ChildInfo>,
) -> bool {
	let new_kv = vec![(key.as_ref(), Some(new_v.as_ref()))];
	let mut stx = match child_info {
		Some(info) => trie.child_storage_root(info, new_kv.iter().cloned(), version).2,
		None => trie.storage_root(new_kv.iter().cloned(), version).1,
	};
	for (mut k, (_, rc)) in stx.drain().into_iter() {
		if rc > 0 {
			db.sanitize_key(&mut k);
			if db.get(col, &k).is_some() {
				trace!("Benchmark-store key creation: Key collision detected, retry");
				return false;
			}
		}
	}
	true
}
