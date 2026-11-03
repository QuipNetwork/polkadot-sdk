// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus. If not, see <https://www.gnu.org/licenses/>.

//! Per-block resubmission store for the unincluded segment.
//!
//! Persists a [`StoredEntry`] per imported parablock, keyed by parablock hash, holding everything
//! needed to reconstruct and resubmit the collation for an unincluded block without assuming the
//! relay parent is still available: the storage proof, the relay parent header and the relay-parent
//! session.
//!
//! Entries are pruned on parachain finality via [`prune_finalized_entries`], which is meant to run
//! on the same task that records entries so a recorded entry is always observed by a subsequent
//! finality notification.

use codec::{Decode, Encode};
use cumulus_primitives_core::relay_chain::{Header as RelayHeader, SessionIndex};
use sc_client_api::{
	backend::AuxStore,
	client::{AuxDataOperations, FinalityNotification},
	HeaderBackend,
};
use sp_blockchain::{Error as ClientError, Result as ClientResult};
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, Zero};
use sp_trie::StorageProof;
use std::{marker::PhantomData, sync::Arc};

const STORE_VERSION_KEY: &[u8] = b"cumulus_resubmission_store_version";
const STORE_CURRENT_VERSION: u32 = 1;
const STORE_ENTRY_PREFIX: &[u8] = b"cumulus_resubmission_store";

/// Entry stored in aux storage for each unincluded parablock.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct StoredEntry {
	/// The storage proof captured at block import/build.
	pub proof: Arc<StorageProof>,
	/// The relay parent header the block was built against, used to determine the relay slot.
	pub relay_parent_header: RelayHeader,
	/// Relay parent's `session_index_for_child`.
	pub relay_parent_session: SessionIndex,
}

fn entry_key<H: Encode>(block_hash: H) -> Vec<u8> {
	(STORE_ENTRY_PREFIX, block_hash).encode()
}

/// Per-block proof store backed by `AuxStore`.
pub struct ResubmissionStore<Block: BlockT, B> {
	backend: Arc<B>,
	_marker: PhantomData<fn() -> Block>,
}

impl<Block: BlockT, B> Clone for ResubmissionStore<Block, B> {
	fn clone(&self) -> Self {
		Self { backend: self.backend.clone(), _marker: PhantomData }
	}
}

impl<Block: BlockT, B> ResubmissionStore<Block, B> {
	/// Create a new store over `backend`.
	pub fn new(backend: Arc<B>) -> Self {
		Self { backend, _marker: PhantomData }
	}
}

/// Build the aux-data key/value pairs to commit alongside a block.
///
/// The caller should push these into `BlockImportParams::auxiliary` so they commit in the
/// same DB transaction as the block. Stateless — no backend access required.
pub fn prepare_resubmission_aux_data<Block: BlockT>(
	block_hash: Block::Hash,
	proof: Arc<StorageProof>,
	relay_parent_header: RelayHeader,
	relay_parent_session: SessionIndex,
) -> impl Iterator<Item = (Vec<u8>, Vec<u8>)> {
	let encoded_entry = (&proof, &relay_parent_header, &relay_parent_session).encode();
	let encoded_version = STORE_CURRENT_VERSION.encode();

	[(entry_key(block_hash), encoded_entry), (STORE_VERSION_KEY.to_vec(), encoded_version)]
		.into_iter()
}

impl<Block: BlockT, B: AuxStore> ResubmissionStore<Block, B> {
	/// Load the entry stored for `block_hash`, if any.
	pub fn load(&self, block_hash: Block::Hash) -> ClientResult<Option<StoredEntry>> {
		let version = self.decode_aux::<u32>(STORE_VERSION_KEY)?;

		match version {
			None => Ok(None),
			Some(STORE_CURRENT_VERSION) => self.decode_aux(entry_key(block_hash).as_slice()),
			Some(other) => Err(ClientError::Backend(format!(
				"Unsupported resubmission store DB version: {:?}",
				other
			))),
		}
	}

	fn decode_aux<T: Decode>(&self, key: &[u8]) -> ClientResult<Option<T>> {
		match self.backend.get_aux(key)? {
			None => Ok(None),
			Some(t) => T::decode(&mut &t[..]).map(Some).map_err(|e| {
				ClientError::Backend(format!(
					"Resubmission store DB is corrupted. Decode error: {}",
					e
				))
			}),
		}
	}
}

/// Delete entries for the just-finalized chain, the tree route, and stale forks — once
/// finalized, a block is no longer in any unincluded segment.
pub fn prune_finalized_entries<Block, B>(
	backend: &B,
	notification: &FinalityNotification<Block>,
) -> ClientResult<()>
where
	Block: BlockT,
	B: AuxStore + HeaderBackend<Block>,
{
	let ops = finality_cleanup_ops::<Block>(
		notification.hash,
		&notification.tree_route,
		notification.stale_blocks.iter().map(|b| b.hash),
	);

	let deletes: Vec<_> =
		ops.iter().filter_map(|(k, v)| v.is_none().then_some(k.as_slice())).collect();

	backend.insert_aux(&[], &deletes)
}

/// Delete entries for blocks that are already finalized, reclaiming any whose prune was never
/// observed — e.g. blocks finalized while the node was down, which the freshly-subscribed
/// notification stream never sees. Run once at startup, before the backfill loop.
pub fn prune_missed_finalized_entries<Block, B>(backend: &B) -> ClientResult<()>
where
	Block: BlockT,
	B: AuxStore + HeaderBackend<Block>,
{
	let mut hash = backend.info().finalized_hash;
	let mut deletes: Vec<Vec<u8>> = Vec::new();

	while let Some(header) = backend.header(hash)? {
		let key = entry_key(hash);
		// First finalized block without an entry: everything below is already pruned.
		if backend.get_aux(&key)?.is_none() {
			break;
		}
		deletes.push(key);
		if header.number().is_zero() {
			break;
		}
		hash = *header.parent_hash();
	}

	if !deletes.is_empty() {
		let delete_refs: Vec<&[u8]> = deletes.iter().map(|k| k.as_slice()).collect();
		backend.insert_aux(&[], &delete_refs)?;
	}

	Ok(())
}

/// Compute aux storage cleanup operations.
///
/// Emits deletes for stale-fork blocks, intermediate tree-route blocks, and the just-finalized
/// block itself. Once a block is finalized it is no longer in any unincluded segment, so its
/// proof entry is dead weight.
fn finality_cleanup_ops<Block: BlockT>(
	just_finalized_hash: Block::Hash,
	tree_route: &[Block::Hash],
	stale_block_hashes: impl IntoIterator<Item = Block::Hash>,
) -> AuxDataOperations {
	let stale_iter = stale_block_hashes.into_iter();

	let mut ops = Vec::with_capacity(stale_iter.size_hint().0 + tree_route.len() + 1);
	ops.extend(stale_iter.map(|hash| (entry_key(hash), None)));
	ops.extend(tree_route.iter().map(|hash| (entry_key(hash), None)));
	ops.push((entry_key(just_finalized_hash), None));

	ops
}

#[cfg(test)]
mod tests {
	use super::*;
	use sc_client_api::backend::AuxStore;

	type Block = substrate_test_runtime::Block;
	type Hash = <Block as BlockT>::Hash;
	type TestBackend = sc_client_api::in_mem::Backend<Block>;
	type Store = ResubmissionStore<Block, TestBackend>;

	fn test_relay_header(number: u32) -> RelayHeader {
		RelayHeader {
			parent_hash: Default::default(),
			number,
			state_root: Default::default(),
			extrinsics_root: Default::default(),
			digest: Default::default(),
		}
	}

	fn create_test_entry() -> StoredEntry {
		StoredEntry {
			proof: Arc::new(StorageProof::new(vec![vec![1, 2, 3], vec![4, 5, 6]])),
			relay_parent_header: test_relay_header(7),
			relay_parent_session: 1,
		}
	}

	fn new_store() -> (Arc<TestBackend>, Store) {
		let backend = Arc::new(TestBackend::new());
		let store = Store::new(backend.clone());
		(backend, store)
	}

	fn write_via_store(backend: &Arc<TestBackend>, hash: Hash, entry: &StoredEntry) {
		let pairs: Vec<_> = prepare_resubmission_aux_data::<Block>(
			hash,
			entry.proof.clone(),
			entry.relay_parent_header.clone(),
			entry.relay_parent_session,
		)
		.collect();
		let insert_pairs: Vec<_> =
			pairs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
		AuxStore::insert_aux(&**backend, &insert_pairs, &[]).expect("aux insert should succeed");
	}

	#[test]
	fn prepare_produces_expected_key_value_pairs() {
		let hash = Hash::repeat_byte(0xAB);
		let proof = Arc::new(StorageProof::new(vec![vec![10, 20, 30]]));
		let relay_parent_header = test_relay_header(42);
		let relay_parent_session = 42;
		let pairs: Vec<_> = prepare_resubmission_aux_data::<Block>(
			hash,
			proof.clone(),
			relay_parent_header.clone(),
			relay_parent_session,
		)
		.collect();

		assert_eq!(pairs.len(), 2);

		let expected_key = (STORE_ENTRY_PREFIX, hash).encode();
		assert_eq!(pairs[0].0, expected_key);

		let decoded_entry =
			StoredEntry::decode(&mut pairs[0].1.as_slice()).expect("entry should decode");
		assert_eq!(decoded_entry.proof, proof);
		assert_eq!(decoded_entry.relay_parent_header, relay_parent_header);
		assert_eq!(decoded_entry.relay_parent_session, relay_parent_session);

		assert_eq!(pairs[1].0, STORE_VERSION_KEY.to_vec());
		let decoded_version =
			u32::decode(&mut pairs[1].1.as_slice()).expect("version should decode");
		assert_eq!(decoded_version, STORE_CURRENT_VERSION);
	}

	#[test]
	fn load_returns_none_when_no_entry_exists() {
		let (_backend, store) = new_store();
		let hash = Hash::repeat_byte(0xEF);

		assert_eq!(store.load(hash).expect("load should succeed"), None);
	}

	#[test]
	fn cleanup_combines_all_categories() {
		let stale_1 = Hash::repeat_byte(0xAA);
		let stale_2 = Hash::repeat_byte(0xBB);
		let route_1 = Hash::repeat_byte(0xC1);
		let route_2 = Hash::repeat_byte(0xC2);
		let just_finalized = Hash::repeat_byte(0xFF);

		let ops =
			finality_cleanup_ops::<Block>(just_finalized, &[route_1, route_2], [stale_1, stale_2]);

		let keys: Vec<_> = ops.iter().map(|(k, _)| k.clone()).collect();

		assert!(keys.contains(&entry_key(stale_1)));
		assert!(keys.contains(&entry_key(stale_2)));
		assert!(keys.contains(&entry_key(route_1)));
		assert!(keys.contains(&entry_key(route_2)));
		assert!(keys.contains(&entry_key(just_finalized)));

		assert!(ops.iter().all(|(_, v)| v.is_none()));
	}

	#[test]
	fn cleanup_handles_empty_inputs() {
		let just_finalized = Hash::repeat_byte(0xFF);

		let ops = finality_cleanup_ops::<Block>(just_finalized, &[], std::iter::empty::<Hash>());

		assert_eq!(ops.len(), 1);
		assert!(ops.iter().all(|(_, v)| v.is_none()));
	}

	#[test]
	fn stored_entry_round_trips() {
		// The on-disk format of `StoredEntry` is versioned by `STORE_CURRENT_VERSION`. If this
		// struct's encoding changes, existing aux entries written by older builds will fail to
		// decode — bump `STORE_CURRENT_VERSION` and add a migration.
		let entry = create_test_entry();

		let encoded = entry.encode();
		let decoded = StoredEntry::decode(&mut encoded.as_slice()).expect("decode should succeed");
		assert_eq!(entry, decoded);
	}

	#[test]
	fn decode_corrupted_entry_body() {
		let (backend, store) = new_store();
		let hash = Hash::repeat_byte(0xAB);

		// Write correct version.
		let version_encoded = STORE_CURRENT_VERSION.encode();
		AuxStore::insert_aux(&*backend, &[(STORE_VERSION_KEY, version_encoded.as_slice())], &[])
			.expect("aux insert should succeed");

		// Write bogus entry body.
		let key = entry_key(hash);
		let bogus_data = vec![0xFF, 0xAA, 0xBB];
		AuxStore::insert_aux(&*backend, &[(&key[..], bogus_data.as_slice())], &[])
			.expect("aux insert should succeed");

		let result = store.load(hash);
		assert!(result.is_err());
		let err_msg = result.unwrap_err().to_string();
		assert!(
			err_msg.contains("DB is corrupted") && err_msg.contains("Decode error"),
			"unexpected error: {}",
			err_msg
		);
	}

	#[test]
	fn end_to_end_write_cleanup_load() {
		let (backend, store) = new_store();

		let hash1 = Hash::repeat_byte(0x01);
		let hash2 = Hash::repeat_byte(0x02);
		let hash3 = Hash::repeat_byte(0x03);

		let entry1 = create_test_entry();
		let entry2 = create_test_entry();
		let entry3 = create_test_entry();

		write_via_store(&backend, hash1, &entry1);
		write_via_store(&backend, hash2, &entry2);
		write_via_store(&backend, hash3, &entry3);

		assert_eq!(store.load(hash1).expect("load"), Some(entry1));
		assert_eq!(store.load(hash2).expect("load"), Some(entry2));
		assert_eq!(store.load(hash3).expect("load"), Some(entry3.clone()));

		// Generate cleanup that deletes hash1 (just-finalized) and hash2 (in tree route).
		let ops = finality_cleanup_ops::<Block>(hash1, &[hash2], std::iter::empty::<Hash>());
		let delete_keys: Vec<_> =
			ops.iter().filter_map(|(k, v)| v.is_none().then(|| k.as_slice())).collect();

		AuxStore::insert_aux(&*backend, &[], &delete_keys).expect("delete should succeed");

		assert_eq!(store.load(hash1).expect("load"), None, "hash1 should be deleted");
		assert_eq!(store.load(hash2).expect("load"), None, "hash2 should be deleted");
		assert_eq!(store.load(hash3).expect("load"), Some(entry3), "hash3 should survive");
	}

	#[test]
	fn entries_survive_disk_restart() {
		use sc_client_db::{
			Backend as DbBackend, BlocksPruning, DatabaseSettings, DatabaseSource, PruningMode,
		};

		fn with_backend<R>(
			path: &std::path::Path,
			f: impl FnOnce(&Arc<DbBackend<Block>>) -> R,
		) -> R {
			let backend = Arc::new(
				DbBackend::<Block>::new(
					DatabaseSettings {
						trie_cache_maximum_size: Some(16 * 1024 * 1024),
						state_pruning: Some(PruningMode::ArchiveAll),
						blocks_pruning: BlocksPruning::KeepAll,
						pruning_filters: Default::default(),
						source: DatabaseSource::ParityDb { path: path.to_path_buf() },
						metrics_registry: None,
					},
					0,
				)
				.expect("open backend"),
			);
			let result = f(&backend);
			// `backend` (and any clones held by the closure) drop here, closing parity-db.
			result
		}

		let tmp = tempfile::tempdir().expect("tempdir");
		let path = tmp.path();

		let hash_a = Hash::repeat_byte(0x10);
		let hash_b = Hash::repeat_byte(0x20);
		let entry_a = create_test_entry();
		let entry_b = create_test_entry();

		// Write `a` and `b` via the same path block-import uses, then close.
		with_backend(path, |backend| {
			let pairs: Vec<_> = prepare_resubmission_aux_data::<Block>(
				hash_a,
				entry_a.proof.clone(),
				entry_a.relay_parent_header.clone(),
				entry_a.relay_parent_session,
			)
			.chain(prepare_resubmission_aux_data::<Block>(
				hash_b,
				entry_b.proof.clone(),
				entry_b.relay_parent_header.clone(),
				entry_b.relay_parent_session,
			))
			.collect();
			let refs: Vec<_> = pairs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
			AuxStore::insert_aux(&**backend, &refs, &[]).expect("aux insert");
		});

		// Restart: confirm both entries survived, then apply a finality-style delete of `a`.
		with_backend(path, |backend| {
			let store = ResubmissionStore::<Block, _>::new(backend.clone());
			assert_eq!(store.load(hash_a).expect("load a"), Some(entry_a.clone()));
			assert_eq!(store.load(hash_b).expect("load b"), Some(entry_b.clone()));

			let ops = finality_cleanup_ops::<Block>(hash_a, &[], std::iter::empty::<Hash>());
			let delete_keys: Vec<_> =
				ops.iter().filter_map(|(k, v)| v.is_none().then(|| k.as_slice())).collect();
			AuxStore::insert_aux(&**backend, &[], &delete_keys).expect("delete");
		});

		// Restart: the delete must have persisted; `b` must still be there.
		with_backend(path, |backend| {
			let store = ResubmissionStore::<Block, _>::new(backend.clone());
			assert_eq!(store.load(hash_a).expect("load a"), None, "hash_a delete must persist");
			assert_eq!(store.load(hash_b).expect("load b"), Some(entry_b));
		});

		// `tmp` drops here, recursively removing the parity-db directory.
	}

	#[test]
	fn load_returns_error_on_unsupported_version() {
		let (backend, store) = new_store();
		let hash = Hash::repeat_byte(0xAB);

		// Write an unsupported version number to the version key.
		let unsupported_version = 99u32;
		let encoded_version = unsupported_version.encode();
		AuxStore::insert_aux(&*backend, &[(STORE_VERSION_KEY, encoded_version.as_slice())], &[])
			.expect("aux insert should succeed");

		let result = store.load(hash);
		assert!(result.is_err());
		let err_msg = result.unwrap_err().to_string();
		assert!(
			err_msg.contains("Unsupported") && err_msg.contains("version"),
			"unexpected error: {}",
			err_msg
		);
	}

	/// A minimal in-memory backend — a finalized chain plus an aux key-value store. Enough to drive
	/// [`prune_missed_finalized_entries`] (which only reads `info()`/`header()` and writes aux)
	/// without a runtime or on-disk DB.
	#[derive(Default)]
	struct MockBackend {
		headers: std::collections::HashMap<Hash, <Block as BlockT>::Header>,
		by_number: std::collections::HashMap<u64, Hash>,
		finalized: (Hash, u64),
		aux: std::sync::Mutex<std::collections::HashMap<Vec<u8>, Vec<u8>>>,
	}

	impl MockBackend {
		/// Append a block on top of `parent`, returning its hash.
		fn push(&mut self, number: u64, parent: Hash) -> Hash {
			let header = <<Block as BlockT>::Header as HeaderT>::new(
				number,
				Default::default(),
				Default::default(),
				parent,
				Default::default(),
			);
			let hash = header.hash();
			self.headers.insert(hash, header);
			self.by_number.insert(number, hash);
			hash
		}

		fn write_entry(&self, hash: Hash, entry: StoredEntry) {
			let pairs: Vec<_> = prepare_resubmission_aux_data::<Block>(
				hash,
				entry.proof,
				entry.relay_parent_header,
				entry.relay_parent_session,
			)
			.collect();
			let refs: Vec<_> = pairs.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
			self.insert_aux(&refs, &[]).unwrap();
		}

		fn has_entry(&self, hash: Hash) -> bool {
			self.get_aux(&entry_key(hash)).unwrap().is_some()
		}
	}

	impl AuxStore for MockBackend {
		fn insert_aux<
			'a,
			'b: 'a,
			'c: 'a,
			I: IntoIterator<Item = &'a (&'c [u8], &'c [u8])>,
			D: IntoIterator<Item = &'a &'b [u8]>,
		>(
			&self,
			insert: I,
			delete: D,
		) -> ClientResult<()> {
			let mut aux = self.aux.lock().unwrap();
			for (k, v) in insert {
				aux.insert(k.to_vec(), v.to_vec());
			}
			for k in delete {
				aux.remove(*k);
			}
			Ok(())
		}

		fn get_aux(&self, key: &[u8]) -> ClientResult<Option<Vec<u8>>> {
			Ok(self.aux.lock().unwrap().get(key).cloned())
		}
	}

	impl HeaderBackend<Block> for MockBackend {
		fn header(&self, hash: Hash) -> ClientResult<Option<<Block as BlockT>::Header>> {
			Ok(self.headers.get(&hash).cloned())
		}

		fn info(&self) -> sp_blockchain::Info<Block> {
			sp_blockchain::Info {
				best_hash: self.finalized.0,
				best_number: self.finalized.1,
				genesis_hash: self.by_number.get(&0).copied().unwrap_or_default(),
				finalized_hash: self.finalized.0,
				finalized_number: self.finalized.1,
				finalized_state: None,
				number_leaves: 1,
				block_gap: None,
			}
		}

		fn status(&self, hash: Hash) -> ClientResult<sp_blockchain::BlockStatus> {
			Ok(if self.headers.contains_key(&hash) {
				sp_blockchain::BlockStatus::InChain
			} else {
				sp_blockchain::BlockStatus::Unknown
			})
		}

		fn number(&self, hash: Hash) -> ClientResult<Option<u64>> {
			Ok(self.headers.get(&hash).map(|h| *h.number()))
		}

		fn hash(&self, number: u64) -> ClientResult<Option<Hash>> {
			Ok(self.by_number.get(&number).cloned())
		}
	}

	#[test]
	fn prune_missed_reclaims_entries_finalized_while_down() {
		let mut backend = MockBackend::default();

		// Genesis plus a 5-block chain; record an entry for each non-genesis block.
		let mut parent = backend.push(0, Default::default());
		let mut hashes = Vec::new();
		for number in 1..=5u64 {
			parent = backend.push(number, parent);
			hashes.push(parent);
		}
		for hash in hashes.iter() {
			backend.write_entry(*hash, create_test_entry());
		}

		// Finalize up to block 3 with nothing pruning (stands in for finalization observed while
		// the node was down): entries for the now-finalized blocks 1..=3 leak, they are still
		// present.
		backend.finalized = (hashes[2], 3);
		assert!(
			backend.has_entry(hashes[0]) &&
				backend.has_entry(hashes[1]) &&
				backend.has_entry(hashes[2]),
			"finalized entries leaked",
		);

		// Startup pruning reclaims the finalized entries and spares the still-unincluded
		// ones. The downward walk stops at genesis (no entry there).
		prune_missed_finalized_entries::<Block, _>(&backend).unwrap();
		assert!(
			!backend.has_entry(hashes[0]) &&
				!backend.has_entry(hashes[1]) &&
				!backend.has_entry(hashes[2]),
			"finalized entries reclaimed",
		);
		assert!(
			backend.has_entry(hashes[3]) && backend.has_entry(hashes[4]),
			"unincluded entries kept",
		);
	}
}
