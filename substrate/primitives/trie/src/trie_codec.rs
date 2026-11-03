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

//! Compact proof support.
//!
//! This uses compact proof from trie crate and extends
//! it to substrate specific layout and child trie system.

use crate::{CompactProof, HashDBT, TrieConfiguration, TrieHash, EMPTY_PREFIX};
use alloc::{boxed::Box, vec::Vec};
use trie_db::{CError, Trie};

/// Error for trie node decoding.
#[derive(Debug)]
#[cfg_attr(feature = "std", derive(thiserror::Error))]
pub enum Error<H, CodecError> {
	#[cfg_attr(feature = "std", error("Invalid root {0:x?}, expected {1:x?}"))]
	RootMismatch(H, H),
	#[cfg_attr(feature = "std", error("Missing nodes in the proof"))]
	IncompleteProof,
	#[cfg_attr(feature = "std", error("Child node content with no root in proof"))]
	ExtraneousChildNode,
	#[cfg_attr(feature = "std", error("Proof of child trie {0:x?} not in parent proof"))]
	ExtraneousChildProof(H),
	#[cfg_attr(feature = "std", error("Invalid root {0:x?}, expected {1:x?}"))]
	InvalidChildRoot(Vec<u8>, Vec<u8>),
	#[cfg_attr(feature = "std", error("Trie error: {0:?}"))]
	TrieError(Box<trie_db::TrieError<H, CodecError>>),
}

impl<H, CodecError> From<Box<trie_db::TrieError<H, CodecError>>> for Error<H, CodecError> {
	fn from(error: Box<trie_db::TrieError<H, CodecError>>) -> Self {
		Error::TrieError(error)
	}
}

/// Decode a compact proof.
///
/// Takes as input a destination `db` for decoded node and `encoded`
/// an iterator of compact encoded nodes.
///
/// The decoded root is always checked against `expected_root`.
///
/// Child trie are decoded in order of child trie root present
/// in the top trie.
pub fn decode_compact<'a, L, DB, I>(
	db: &mut DB,
	encoded: I,
	expected_root: &TrieHash<L>,
) -> Result<TrieHash<L>, Error<TrieHash<L>, CError<L>>>
where
	L: TrieConfiguration,
	DB: HashDBT<L::Hash, trie_db::DBValue> + hash_db::HashDBRef<L::Hash, trie_db::DBValue>,
	I: IntoIterator<Item = &'a [u8]>,
{
	let mut nodes_iter = encoded.into_iter();
	let (top_root, _nb_used) = trie_db::decode_compact_from_iter::<L, _, _>(db, &mut nodes_iter)?;

	if &top_root != expected_root {
		return Err(Error::RootMismatch(top_root, *expected_root));
	}

	let mut child_tries = Vec::new();
	{
		// fetch child trie roots
		let trie = crate::TrieDBBuilder::<L>::new(db, &top_root).build();

		let mut iter = trie.iter()?;

		let childtrie_roots = sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX;
		if iter.seek(childtrie_roots).is_ok() {
			loop {
				match iter.next() {
					Some(Ok((key, value))) if key.starts_with(childtrie_roots) => {
						// we expect all default child trie root to be correctly encoded.
						// see other child trie functions.
						let mut root = TrieHash::<L>::default();
						// still in a proof so prevent panic
						if root.as_mut().len() != value.as_slice().len() {
							return Err(Error::InvalidChildRoot(key, value));
						}
						root.as_mut().copy_from_slice(value.as_ref());
						child_tries.push(root);
					},
					// allow incomplete database error: we only
					// require access to data in the proof.
					Some(Err(error)) => match *error {
						trie_db::TrieError::IncompleteDatabase(..) => (),
						e => return Err(Box::new(e).into()),
					},
					_ => break,
				}
			}
		}
	}

	if !HashDBT::<L::Hash, _>::contains(db, &top_root, EMPTY_PREFIX) {
		return Err(Error::IncompleteProof);
	}

	let mut previous_extracted_child_trie = None;
	let mut nodes_iter = nodes_iter.peekable();
	for child_root in child_tries.into_iter() {
		if previous_extracted_child_trie.is_none() && nodes_iter.peek().is_some() {
			let (top_root, _) = trie_db::decode_compact_from_iter::<L, _, _>(db, &mut nodes_iter)?;
			previous_extracted_child_trie = Some(top_root);
		}

		// we do not early exit on root mismatch but try the
		// other read from proof (some child root may be
		// in proof without actual child content).
		if Some(child_root) == previous_extracted_child_trie {
			previous_extracted_child_trie = None;
		}
	}

	if let Some(child_root) = previous_extracted_child_trie {
		// A child root was read from proof but is not present
		// in top trie.
		return Err(Error::ExtraneousChildProof(child_root));
	}

	if nodes_iter.next().is_some() {
		return Err(Error::ExtraneousChildNode);
	}

	Ok(top_root)
}

/// Encode a compact proof.
///
/// Takes as input all full encoded node from the proof, and
/// the root.
/// Then parse all child trie root and compress main trie content first
/// then all child trie contents.
/// Child trie are ordered by the order of their roots in the top trie.
pub fn encode_compact<L, DB>(
	partial_db: &DB,
	root: &TrieHash<L>,
) -> Result<CompactProof, Error<TrieHash<L>, CError<L>>>
where
	L: TrieConfiguration,
	DB: HashDBT<L::Hash, trie_db::DBValue> + hash_db::HashDBRef<L::Hash, trie_db::DBValue>,
{
	let mut seen = trie_db::SeenHashes::<L>::default();
	let mut child_tries = Vec::new();
	let mut compact_proof = {
		let trie = crate::TrieDBBuilder::<L>::new(partial_db, root).build();

		let mut iter = trie.iter()?;

		let childtrie_roots = sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX;
		if iter.seek(childtrie_roots).is_ok() {
			loop {
				match iter.next() {
					Some(Ok((key, value))) if key.starts_with(childtrie_roots) => {
						let mut root = TrieHash::<L>::default();
						if root.as_mut().len() != value.as_slice().len() {
							// some child trie root in top trie are not an encoded hash.
							return Err(Error::InvalidChildRoot(key.to_vec(), value.to_vec()));
						}
						root.as_mut().copy_from_slice(value.as_ref());
						child_tries.push(root);
					},
					// allow incomplete database error: we only
					// require access to data in the proof.
					Some(Err(error)) => match *error {
						trie_db::TrieError::IncompleteDatabase(..) => (),
						e => return Err(Box::new(e).into()),
					},
					_ => break,
				}
			}
		}

		trie_db::encode_compact_skip_duplicates::<L>(&trie, &mut seen)?
	};

	for child_root in child_tries {
		if !HashDBT::<L::Hash, _>::contains(partial_db, &child_root, EMPTY_PREFIX) {
			// child proof are allowed to be missing (unused root can be included
			// due to trie structure modification).
			continue;
		}

		let trie = crate::TrieDBBuilder::<L>::new(partial_db, &child_root).build();
		let child_proof = trie_db::encode_compact_skip_duplicates::<L>(&trie, &mut seen)?;

		compact_proof.extend(child_proof);
	}

	Ok(CompactProof { encoded_nodes: compact_proof })
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{delta_trie_root, recorder::IgnoredNodes, HashDB, StorageProof};
	use codec::Encode;
	use hash_db::AsHashDB;
	use sp_core::{Blake2Hasher, H256};
	use std::collections::HashSet;
	use trie_db::{DBValue, Trie, TrieDBBuilder, TrieDBMutBuilder, TrieHash, TrieMut};

	type MemoryDB = crate::MemoryDB<sp_core::Blake2Hasher>;
	type Layout = crate::LayoutV1<sp_core::Blake2Hasher>;
	type Recorder = crate::recorder::Recorder<sp_core::Blake2Hasher>;

	fn create_trie(num_keys: u32) -> (MemoryDB, TrieHash<Layout>) {
		let mut db = MemoryDB::default();
		let mut root = Default::default();

		{
			let mut trie = TrieDBMutBuilder::<Layout>::new(&mut db, &mut root).build();
			for i in 0..num_keys {
				trie.insert(
					&i.encode(),
					&vec![1u8; 64].into_iter().chain(i.encode()).collect::<Vec<_>>(),
				)
				.expect("Inserts data");
			}
		}

		(db, root)
	}

	#[test]
	fn values_shared_between_top_and_child_trie_are_deduplicated() {
		let mut db = MemoryDB::default();

		// A value above `TRIE_VALUE_NODE_THRESHOLD` that is stored as a separate value node
		// referenced by hash. It is present in the top trie and in a child trie; the
		// deduplication state must be shared across the per-trie encodings so it is only
		// emitted once in the whole proof.
		let shared_value = vec![42u8; 64];

		let mut child_root = Default::default();
		{
			let mut trie = TrieDBMutBuilder::<Layout>::new(&mut db, &mut child_root).build();
			trie.insert(b"child_key", &shared_value).expect("Inserts data");
		}

		let mut root = Default::default();
		{
			let mut trie = TrieDBMutBuilder::<Layout>::new(&mut db, &mut root).build();
			trie.insert(b"top_key", &shared_value).expect("Inserts data");

			let mut child_storage_key =
				sp_core::storage::well_known_keys::DEFAULT_CHILD_STORAGE_KEY_PREFIX.to_vec();
			child_storage_key.extend(b"child1");
			trie.insert(&child_storage_key, child_root.as_ref()).expect("Inserts data");
		}

		let compact_proof = encode_compact::<Layout, _>(&db, &root).unwrap();

		// The shared value must only be contained once in the compact proof, even though the
		// top trie and the child trie are encoded separately.
		let occurrences = compact_proof
			.encoded_nodes
			.iter()
			.filter(|node| node.as_slice() == shared_value.as_slice())
			.count();
		assert_eq!(occurrences, 1);

		// The proof still decodes and gives access to the value through both tries.
		let mut res_db = MemoryDB::new(&[]);
		decode_compact::<Layout, _, _>(
			&mut res_db,
			compact_proof.iter_compact_encoded_nodes(),
			&root,
		)
		.unwrap();

		let trie = TrieDBBuilder::<Layout>::new(&res_db, &root).build();
		assert_eq!(trie.get(b"top_key").unwrap().unwrap(), shared_value);

		let child_trie = TrieDBBuilder::<Layout>::new(&res_db, &child_root).build();
		assert_eq!(child_trie.get(b"child_key").unwrap().unwrap(), shared_value);
	}

	struct Overlay<'a> {
		db: &'a MemoryDB,
		write: MemoryDB,
	}

	impl hash_db::HashDB<sp_core::Blake2Hasher, DBValue> for Overlay<'_> {
		fn get(
			&self,
			key: &<sp_core::Blake2Hasher as hash_db::Hasher>::Out,
			prefix: hash_db::Prefix,
		) -> Option<DBValue> {
			HashDB::get(self.db, key, prefix)
		}

		fn contains(
			&self,
			key: &<sp_core::Blake2Hasher as hash_db::Hasher>::Out,
			prefix: hash_db::Prefix,
		) -> bool {
			HashDB::contains(self.db, key, prefix)
		}

		fn insert(
			&mut self,
			prefix: hash_db::Prefix,
			value: &[u8],
		) -> <sp_core::Blake2Hasher as hash_db::Hasher>::Out {
			self.write.insert(prefix, value)
		}

		fn emplace(
			&mut self,
			key: <sp_core::Blake2Hasher as hash_db::Hasher>::Out,
			prefix: hash_db::Prefix,
			value: DBValue,
		) {
			self.write.emplace(key, prefix, value);
		}

		fn remove(
			&mut self,
			key: &<sp_core::Blake2Hasher as hash_db::Hasher>::Out,
			prefix: hash_db::Prefix,
		) {
			self.write.remove(key, prefix);
		}
	}

	impl AsHashDB<Blake2Hasher, DBValue> for Overlay<'_> {
		fn as_hash_db(&self) -> &dyn HashDBT<Blake2Hasher, DBValue> {
			self
		}

		fn as_hash_db_mut<'a>(&'a mut self) -> &'a mut (dyn HashDBT<Blake2Hasher, DBValue> + 'a) {
			self
		}
	}

	fn emulate_block_building(
		state: &MemoryDB,
		root: H256,
		read_keys: &[u32],
		write_keys: &[u32],
		nodes_to_ignore: IgnoredNodes<H256>,
	) -> (Recorder, MemoryDB, H256) {
		let recorder = Recorder::with_ignored_nodes(nodes_to_ignore);

		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(state, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			for key in read_keys {
				trie.get(&key.encode()).unwrap().unwrap();
			}
		}

		let mut overlay = Overlay { db: state, write: Default::default() };

		let new_root = {
			let mut trie_recorder = recorder.as_trie_recorder(root);
			delta_trie_root::<Layout, _, _, _, _, _>(
				&mut overlay,
				root,
				write_keys.iter().map(|k| {
					(
						k.encode(),
						Some(vec![2u8; 64].into_iter().chain(k.encode()).collect::<Vec<_>>()),
					)
				}),
				Some(&mut trie_recorder),
				None,
			)
			.unwrap()
		};

		(recorder, overlay.write, new_root)
	}

	fn build_known_nodes_list(recorder: &Recorder, transaction: &MemoryDB) -> IgnoredNodes<H256> {
		let mut ignored_nodes =
			IgnoredNodes::from_storage_proof::<Blake2Hasher>(&recorder.to_storage_proof());

		ignored_nodes.extend(IgnoredNodes::from_memory_db::<Blake2Hasher, _>(transaction.clone()));

		ignored_nodes
	}

	#[test]
	fn ensure_multiple_tries_encode_compact_works() {
		let (mut db, root) = create_trie(100);

		let mut nodes_to_ignore = IgnoredNodes::default();
		let (recorder, transaction, root1) = emulate_block_building(
			&db,
			root,
			&[2, 4, 5, 6, 7, 8],
			&[9, 10, 11, 12, 13, 14],
			nodes_to_ignore.clone(),
		);

		db.consolidate(transaction.clone());
		nodes_to_ignore.extend(build_known_nodes_list(&recorder, &transaction));

		let (recorder2, transaction, root2) = emulate_block_building(
			&db,
			root1,
			&[9, 10, 11, 12, 13, 14],
			&[15, 16, 17, 18, 19, 20],
			nodes_to_ignore.clone(),
		);

		db.consolidate(transaction.clone());
		nodes_to_ignore.extend(build_known_nodes_list(&recorder2, &transaction));

		let (recorder3, _, root3) = emulate_block_building(
			&db,
			root2,
			&[20, 30, 40, 41, 42],
			&[80, 90, 91, 92, 93],
			nodes_to_ignore,
		);

		let proof = recorder.to_storage_proof();
		let proof2 = recorder2.to_storage_proof();
		let proof3 = recorder3.to_storage_proof();

		let mut combined = HashSet::<Vec<u8>>::from_iter(proof.into_iter_nodes());
		proof2.iter_nodes().for_each(|n| assert!(combined.insert(n.clone())));
		proof3.iter_nodes().for_each(|n| assert!(combined.insert(n.clone())));

		let proof = StorageProof::new(combined.into_iter());

		let compact_proof = encode_compact::<Layout, _>(&proof.to_memory_db(), &root).unwrap();

		assert!(proof.encoded_size() > compact_proof.encoded_size());

		let mut res_db = crate::MemoryDB::<Blake2Hasher>::new(&[]);
		decode_compact::<Layout, _, _>(
			&mut res_db,
			compact_proof.iter_compact_encoded_nodes(),
			&root,
		)
		.unwrap();

		let (_, transaction, root1_proof) = emulate_block_building(
			&res_db,
			root,
			&[2, 4, 5, 6, 7, 8],
			&[9, 10, 11, 12, 13, 14],
			Default::default(),
		);

		assert_eq!(root1, root1_proof);

		res_db.consolidate(transaction);

		let (_, transaction2, root2_proof) = emulate_block_building(
			&res_db,
			root1,
			&[9, 10, 11, 12, 13, 14],
			&[15, 16, 17, 18, 19, 20],
			Default::default(),
		);

		assert_eq!(root2, root2_proof);

		res_db.consolidate(transaction2);

		let (_, _, root3_proof) = emulate_block_building(
			&res_db,
			root2,
			&[20, 30, 40, 41, 42],
			&[80, 90, 91, 92, 93],
			Default::default(),
		);

		assert_eq!(root3, root3_proof);
	}
}
