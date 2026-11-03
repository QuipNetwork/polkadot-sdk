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

//! Trie recorder
//!
//! Provides an implementation of the [`TrieRecorder`](trie_db::TrieRecorder) trait. It can be used
//! to record storage accesses to the state to generate a [`StorageProof`].

use crate::{GenericMemoryDB, NodeCodec, StorageProof};
use codec::{Compact, Decode, Encode};
use hash_db::Hasher;
use memory_db::KeyFunction;
use parking_lot::{Mutex, MutexGuard};
use std::{
	collections::{HashMap, HashSet},
	marker::PhantomData,
	mem,
	ops::DerefMut,
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
};
use trie_db::{RecordedForKey, TrieAccess};

const LOG_TARGET: &str = "trie-recorder";

/// A list of ignored nodes for [`Recorder`].
///
/// These nodes when passed to a recorder will be ignored and not recorded by the recorder.
#[derive(Clone, Debug)]
pub struct IgnoredNodes<H> {
	nodes: HashSet<H>,
}

impl<H: Encode> Encode for IgnoredNodes<H> {
	fn encode(&self) -> Vec<u8> {
		let mut encoded = Compact::<u32>(self.nodes.len() as _).encode();
		self.nodes.iter().for_each(|n| n.encode_to(&mut encoded));
		encoded
	}
}

impl<H: Decode + std::hash::Hash + Eq> Decode for IgnoredNodes<H> {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let len = Compact::<u32>::decode(input)?;
		let data = codec::decode_vec_with_len(input, len.0 as _)?;
		Ok(Self { nodes: HashSet::from_iter(data.into_iter()) })
	}
}

impl<H> Default for IgnoredNodes<H> {
	fn default() -> Self {
		Self { nodes: HashSet::default() }
	}
}

impl<H: Eq + std::hash::Hash + Clone> IgnoredNodes<H> {
	/// Initialize from the given storage proof.
	///
	/// So, all recorded nodes of the proof will be the ignored nodes.
	pub fn from_storage_proof<Hasher: trie_db::Hasher<Out = H>>(proof: &StorageProof) -> Self {
		Self { nodes: proof.iter_nodes().map(|n| Hasher::hash(&n)).collect() }
	}

	/// Initialize from the given memory db.
	///
	/// All nodes that have a reference count > 0 will be used as ignored nodes.
	pub fn from_memory_db<Hasher: trie_db::Hasher<Out = H>, KF: KeyFunction<Hasher>>(
		mut memory_db: GenericMemoryDB<Hasher, KF>,
	) -> Self {
		Self {
			nodes: memory_db
				.drain()
				.into_iter()
				// We do not want to add removed nodes.
				.filter(|(_, (_, counter))| *counter > 0)
				.map(|(_, (data, _))| Hasher::hash(&data))
				.collect(),
		}
	}

	/// Extend `self` with the other instance of ignored nodes.
	pub fn extend(&mut self, other: Self) {
		self.nodes.extend(other.nodes.into_iter());
	}

	/// Returns `true` if the node is ignored.
	pub fn is_ignored(&self, node: &H) -> bool {
		self.nodes.contains(node)
	}
}

/// Stores all the information per transaction.
#[derive(Default)]
struct Transaction<H> {
	/// Stores transaction information about [`RecorderInner::recorded_keys`].
	///
	/// For each transaction we only store the `storage_root` and the old states per key. `None`
	/// state means that the key wasn't recorded before.
	recorded_keys: HashMap<H, HashMap<Arc<[u8]>, Option<RecordedForKey>>>,
	/// Stores transaction information about [`RecorderInner::accessed_nodes`].
	///
	/// For each transaction we only store the hashes of added nodes.
	accessed_nodes: HashSet<H>,
}

/// The internals of [`Recorder`].
struct RecorderInner<H> {
	/// The keys for that we have recorded the trie nodes and if we have recorded up to the value.
	///
	/// Mapping: `StorageRoot -> (Key -> RecordedForKey)`.
	recorded_keys: HashMap<H, HashMap<Arc<[u8]>, RecordedForKey>>,

	/// Currently active transactions.
	transactions: Vec<Transaction<H>>,

	/// The encoded nodes we accessed while recording.
	///
	/// Mapping: `Hash(Node) -> Node`.
	accessed_nodes: HashMap<H, Vec<u8>>,

	/// Nodes that should be ignored and not recorded.
	ignored_nodes: IgnoredNodes<H>,
}

impl<H> Default for RecorderInner<H> {
	fn default() -> Self {
		Self {
			recorded_keys: Default::default(),
			accessed_nodes: Default::default(),
			transactions: Vec::new(),
			ignored_nodes: Default::default(),
		}
	}
}

/// The trie recorder.
///
/// Owns the recorded data. Is used to transform data into a storage
/// proof and to provide transaction support. The `as_trie_recorder` method provides a
/// [`trie_db::TrieDB`] compatible recorder that implements the actual recording logic.
pub struct Recorder<H: Hasher> {
	inner: Arc<Mutex<RecorderInner<H::Out>>>,
	/// The estimated encoded size of the storage proof this recorder will produce.
	///
	/// We store this in an atomic to be able to fetch the value while the `inner` is may locked.
	encoded_size_estimation: Arc<AtomicUsize>,
}

impl<H: Hasher> Default for Recorder<H> {
	fn default() -> Self {
		Self { inner: Default::default(), encoded_size_estimation: Arc::new(0.into()) }
	}
}

impl<H: Hasher> Clone for Recorder<H> {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			encoded_size_estimation: self.encoded_size_estimation.clone(),
		}
	}
}

impl<H: Hasher> Recorder<H> {
	/// Create a new instance with the given `ingored_nodes`.
	///
	/// These ignored nodes are not recorded when accessed.
	pub fn with_ignored_nodes(ignored_nodes: IgnoredNodes<H::Out>) -> Self {
		Self {
			inner: Arc::new(Mutex::new(RecorderInner { ignored_nodes, ..Default::default() })),
			..Default::default()
		}
	}

	/// Returns [`RecordedForKey`] per recorded key per trie.
	///
	/// There are multiple tries when working with e.g. child tries.
	pub fn recorded_keys(&self) -> HashMap<H::Out, HashMap<Arc<[u8]>, RecordedForKey>> {
		self.inner.lock().recorded_keys.clone()
	}

	/// Returns the recorder as [`TrieRecorder`](trie_db::TrieRecorder) compatible type.
	///
	/// - `storage_root`: The storage root of the trie for which accesses are recorded. This is
	///   important when recording access to different tries at once (like top and child tries).
	///
	/// NOTE: This locks a mutex that stays locked until the return value is dropped.
	#[inline]
	pub fn as_trie_recorder(&self, storage_root: H::Out) -> TrieRecorder<'_, H> {
		TrieRecorder::<H> {
			inner: self.inner.lock(),
			storage_root,
			encoded_size_estimation: self.encoded_size_estimation.clone(),
			_phantom: PhantomData,
		}
	}

	/// Drain the recording into a [`StorageProof`].
	///
	/// While a recorder can be cloned, all share the same internal state. After calling this
	/// function, all other instances will have their internal state reset as well.
	///
	/// If you don't want to drain the recorded state, use [`Self::to_storage_proof`].
	///
	/// Returns the [`StorageProof`].
	pub fn drain_storage_proof(self) -> StorageProof {
		let mut recorder = mem::take(&mut *self.inner.lock());
		StorageProof::new(recorder.accessed_nodes.drain().map(|(_, v)| v))
	}

	/// Convert the recording to a [`StorageProof`].
	///
	/// In contrast to [`Self::drain_storage_proof`] this doesn't consume and doesn't clear the
	/// recordings.
	///
	/// Returns the [`StorageProof`].
	pub fn to_storage_proof(&self) -> StorageProof {
		let recorder = self.inner.lock();
		StorageProof::new(recorder.accessed_nodes.values().cloned())
	}

	/// Returns the estimated encoded size of the proof.
	///
	/// The estimation is based on all the nodes that were accessed until now while
	/// accessing the trie.
	pub fn estimate_encoded_size(&self) -> usize {
		self.encoded_size_estimation.load(Ordering::Relaxed)
	}

	/// Reset the state.
	///
	/// This discards all recorded data.
	pub fn reset(&self) {
		mem::take(&mut *self.inner.lock());
		self.encoded_size_estimation.store(0, Ordering::Relaxed);
	}

	/// Start a new transaction.
	pub fn start_transaction(&self) {
		let mut inner = self.inner.lock();
		inner.transactions.push(Default::default());
	}

	/// Rollback the latest transaction.
	///
	/// Returns an error if there wasn't any active transaction.
	pub fn rollback_transaction(&self) -> Result<(), ()> {
		let mut inner = self.inner.lock();

		// We locked `inner` and can just update the encoded size locally and then store it back to
		// the atomic.
		let mut new_encoded_size_estimation = self.encoded_size_estimation.load(Ordering::Relaxed);
		let transaction = inner.transactions.pop().ok_or(())?;

		transaction.accessed_nodes.into_iter().for_each(|n| {
			if let Some(old) = inner.accessed_nodes.remove(&n) {
				new_encoded_size_estimation =
					new_encoded_size_estimation.saturating_sub(old.encoded_size());
			}
		});

		transaction.recorded_keys.into_iter().for_each(|(storage_root, keys)| {
			keys.into_iter().for_each(|(k, old_state)| {
				if let Some(state) = old_state {
					inner.recorded_keys.entry(storage_root).or_default().insert(k, state);
				} else {
					inner.recorded_keys.entry(storage_root).or_default().remove(&k);
				}
			});
		});

		self.encoded_size_estimation
			.store(new_encoded_size_estimation, Ordering::Relaxed);

		Ok(())
	}

	/// Commit the latest transaction.
	///
	/// Returns an error if there wasn't any active transaction.
	pub fn commit_transaction(&self) -> Result<(), ()> {
		let mut inner = self.inner.lock();

		let transaction = inner.transactions.pop().ok_or(())?;

		if let Some(parent_transaction) = inner.transactions.last_mut() {
			parent_transaction.accessed_nodes.extend(transaction.accessed_nodes);

			transaction.recorded_keys.into_iter().for_each(|(storage_root, keys)| {
				keys.into_iter().for_each(|(k, old_state)| {
					parent_transaction
						.recorded_keys
						.entry(storage_root)
						.or_default()
						.entry(k)
						.or_insert(old_state);
				})
			});
		}

		Ok(())
	}
}

impl<H: Hasher> crate::ProofSizeProvider for Recorder<H> {
	fn estimate_encoded_size(&self) -> usize {
		Recorder::estimate_encoded_size(self)
	}
}

/// The [`TrieRecorder`](trie_db::TrieRecorder) implementation.
pub struct TrieRecorder<'a, H: Hasher> {
	inner: MutexGuard<'a, RecorderInner<H::Out>>,
	storage_root: H::Out,
	encoded_size_estimation: Arc<AtomicUsize>,
	_phantom: PhantomData<H>,
}

impl<H: Hasher> crate::TrieRecorderProvider<H> for Recorder<H> {
	type Recorder<'a>
		= TrieRecorder<'a, H>
	where
		H: 'a;

	fn drain_storage_proof(self) -> Option<StorageProof> {
		Some(Recorder::drain_storage_proof(self))
	}

	fn as_trie_recorder(&self, storage_root: H::Out) -> Self::Recorder<'_> {
		Recorder::as_trie_recorder(&self, storage_root)
	}
}

impl<'a, H: Hasher> TrieRecorder<'a, H> {
	/// Update the recorded keys entry for the given `full_key`.
	fn update_recorded_keys(&mut self, full_key: &[u8], access: RecordedForKey) {
		let inner = self.inner.deref_mut();

		let entry =
			inner.recorded_keys.entry(self.storage_root).or_default().entry(full_key.into());

		let key = entry.key().clone();

		// We don't need to update the record if we only accessed the `Hash` for the given
		// `full_key`. Only `Value` access can be an upgrade from `Hash`.
		let entry = if matches!(access, RecordedForKey::Value) {
			entry.and_modify(|e| {
				if let Some(tx) = inner.transactions.last_mut() {
					// Store the previous state only once per transaction.
					tx.recorded_keys
						.entry(self.storage_root)
						.or_default()
						.entry(key.clone())
						.or_insert(Some(*e));
				}

				*e = access;
			})
		} else {
			entry
		};

		entry.or_insert_with(|| {
			if let Some(tx) = inner.transactions.last_mut() {
				// The key wasn't yet recorded, so there isn't any old state.
				tx.recorded_keys
					.entry(self.storage_root)
					.or_default()
					.entry(key)
					.or_insert(None);
			}

			access
		});
	}
}

impl<'a, H: Hasher> trie_db::TrieRecorder<H::Out> for TrieRecorder<'a, H> {
	fn record(&mut self, access: TrieAccess<H::Out>) {
		let mut encoded_size_update = 0;

		match access {
			TrieAccess::NodeOwned { hash, node_owned } => {
				let inner = self.inner.deref_mut();

				if inner.ignored_nodes.is_ignored(&hash) {
					tracing::trace!(
						target: LOG_TARGET,
						?hash,
						"Ignoring node",
					);
					return;
				}

				tracing::trace!(
					target: LOG_TARGET,
					?hash,
					"Recording node",
				);

				inner.accessed_nodes.entry(hash).or_insert_with(|| {
					let node = node_owned.to_encoded::<NodeCodec<H>>();

					encoded_size_update += node.encoded_size();

					if let Some(tx) = inner.transactions.last_mut() {
						tx.accessed_nodes.insert(hash);
					}

					node
				});
			},
			TrieAccess::EncodedNode { hash, encoded_node } => {
				let inner = self.inner.deref_mut();

				if inner.ignored_nodes.is_ignored(&hash) {
					tracing::trace!(
						target: LOG_TARGET,
						?hash,
						"Ignoring node",
					);
					return;
				}

				tracing::trace!(
					target: LOG_TARGET,
					hash = ?hash,
					"Recording node",
				);

				inner.accessed_nodes.entry(hash).or_insert_with(|| {
					let node = encoded_node.into_owned();

					encoded_size_update += node.encoded_size();

					if let Some(tx) = inner.transactions.last_mut() {
						tx.accessed_nodes.insert(hash);
					}

					node
				});
			},
			TrieAccess::Value { hash, value, full_key } => {
				let inner = self.inner.deref_mut();

				// A value is also just a node.
				if inner.ignored_nodes.is_ignored(&hash) {
					tracing::trace!(
						target: LOG_TARGET,
						?hash,
						"Ignoring value",
					);
					return;
				}

				tracing::trace!(
					target: LOG_TARGET,
					hash = ?hash,
					key = ?sp_core::hexdisplay::HexDisplay::from(&full_key),
					"Recording value",
				);

				inner.accessed_nodes.entry(hash).or_insert_with(|| {
					let value = value.into_owned();

					encoded_size_update += value.encoded_size();

					if let Some(tx) = inner.transactions.last_mut() {
						tx.accessed_nodes.insert(hash);
					}

					value
				});

				self.update_recorded_keys(full_key, RecordedForKey::Value);
			},
			TrieAccess::Hash { full_key } => {
				tracing::trace!(
					target: LOG_TARGET,
					key = ?sp_core::hexdisplay::HexDisplay::from(&full_key),
					"Recorded hash access for key",
				);

				// We don't need to update the `encoded_size_update` as the hash was already
				// accounted for by the recorded node that holds the hash.
				self.update_recorded_keys(full_key, RecordedForKey::Hash);
			},
			TrieAccess::NonExisting { full_key } => {
				tracing::trace!(
					target: LOG_TARGET,
					key = ?sp_core::hexdisplay::HexDisplay::from(&full_key),
					"Recorded non-existing value access for key",
				);

				// Non-existing access means we recorded all trie nodes up to the value.
				// Not the actual value, as it doesn't exist, but all trie nodes to know
				// that the value doesn't exist in the trie.
				self.update_recorded_keys(full_key, RecordedForKey::Value);
			},
			TrieAccess::InlineValue { full_key } => {
				tracing::trace!(
					target: LOG_TARGET,
					key = ?sp_core::hexdisplay::HexDisplay::from(&full_key),
					"Recorded inline value access for key",
				);

				// A value was accessed that is stored inline a node and we recorded all trie nodes
				// to access this value.
				self.update_recorded_keys(full_key, RecordedForKey::Value);
			},
		};

		self.encoded_size_estimation.fetch_add(encoded_size_update, Ordering::Relaxed);
	}

	fn trie_nodes_recorded_for_key(&self, key: &[u8]) -> RecordedForKey {
		self.inner
			.recorded_keys
			.get(&self.storage_root)
			.and_then(|k| k.get(key).copied())
			.unwrap_or(RecordedForKey::None)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::tests::create_trie;
	use trie_db::{Trie, TrieDBBuilder, TrieRecorder};

	type MemoryDB = crate::MemoryDB<sp_core::Blake2Hasher>;
	type Layout = crate::LayoutV1<sp_core::Blake2Hasher>;
	type Recorder = super::Recorder<sp_core::Blake2Hasher>;

	const TEST_DATA: &[(&[u8], &[u8])] =
		&[(b"key1", &[1; 64]), (b"key2", &[2; 64]), (b"key3", &[3; 64]), (b"key4", &[4; 64])];

	#[test]
	fn recorder_works() {
		let (db, root) = create_trie::<Layout>(TEST_DATA);

		let recorder = Recorder::default();

		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();
			assert_eq!(TEST_DATA[0].1.to_vec(), trie.get(TEST_DATA[0].0).unwrap().unwrap());
		}

		let storage_proof = recorder.drain_storage_proof();
		let memory_db: MemoryDB = storage_proof.into_memory_db();

		// Check that we recorded the required data
		let trie = TrieDBBuilder::<Layout>::new(&memory_db, &root).build();
		assert_eq!(TEST_DATA[0].1.to_vec(), trie.get(TEST_DATA[0].0).unwrap().unwrap());
	}

	#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
	struct RecorderStats {
		accessed_nodes: usize,
		recorded_keys: usize,
		estimated_size: usize,
	}

	impl RecorderStats {
		fn extract(recorder: &Recorder) -> Self {
			let inner = recorder.inner.lock();

			let recorded_keys =
				inner.recorded_keys.iter().flat_map(|(_, keys)| keys.keys()).count();

			Self {
				recorded_keys,
				accessed_nodes: inner.accessed_nodes.len(),
				estimated_size: recorder.estimate_encoded_size(),
			}
		}
	}

	#[test]
	fn recorder_transactions_rollback_work() {
		let (db, root) = create_trie::<Layout>(TEST_DATA);

		let recorder = Recorder::default();
		let mut stats = vec![RecorderStats::default()];

		for i in 0..4 {
			recorder.start_transaction();
			{
				let mut trie_recorder = recorder.as_trie_recorder(root);
				let trie = TrieDBBuilder::<Layout>::new(&db, &root)
					.with_recorder(&mut trie_recorder)
					.build();

				assert_eq!(TEST_DATA[i].1.to_vec(), trie.get(TEST_DATA[i].0).unwrap().unwrap());
			}
			stats.push(RecorderStats::extract(&recorder));
		}

		assert_eq!(4, recorder.inner.lock().transactions.len());

		for i in 0..5 {
			assert_eq!(stats[4 - i], RecorderStats::extract(&recorder));

			let storage_proof = recorder.to_storage_proof();
			let memory_db: MemoryDB = storage_proof.into_memory_db();

			// Check that we recorded the required data
			let trie = TrieDBBuilder::<Layout>::new(&memory_db, &root).build();

			// Check that the required data is still present.
			for a in 0..4 {
				if a < 4 - i {
					assert_eq!(TEST_DATA[a].1.to_vec(), trie.get(TEST_DATA[a].0).unwrap().unwrap());
				} else {
					// All the data that we already rolled back, should be gone!
					assert!(trie.get(TEST_DATA[a].0).is_err());
				}
			}

			if i < 4 {
				recorder.rollback_transaction().unwrap();
			}
		}

		assert_eq!(0, recorder.inner.lock().transactions.len());
	}

	#[test]
	fn recorder_transactions_commit_work() {
		let (db, root) = create_trie::<Layout>(TEST_DATA);

		let recorder = Recorder::default();

		for i in 0..4 {
			recorder.start_transaction();
			{
				let mut trie_recorder = recorder.as_trie_recorder(root);
				let trie = TrieDBBuilder::<Layout>::new(&db, &root)
					.with_recorder(&mut trie_recorder)
					.build();

				assert_eq!(TEST_DATA[i].1.to_vec(), trie.get(TEST_DATA[i].0).unwrap().unwrap());
			}
		}

		let stats = RecorderStats::extract(&recorder);
		assert_eq!(4, recorder.inner.lock().transactions.len());

		for _ in 0..4 {
			recorder.commit_transaction().unwrap();
		}
		assert_eq!(0, recorder.inner.lock().transactions.len());
		assert_eq!(stats, RecorderStats::extract(&recorder));

		let storage_proof = recorder.to_storage_proof();
		let memory_db: MemoryDB = storage_proof.into_memory_db();

		// Check that we recorded the required data
		let trie = TrieDBBuilder::<Layout>::new(&memory_db, &root).build();

		// Check that the required data is still present.
		for i in 0..4 {
			assert_eq!(TEST_DATA[i].1.to_vec(), trie.get(TEST_DATA[i].0).unwrap().unwrap());
		}
	}

	#[test]
	fn recorder_transactions_commit_and_rollback_work() {
		let (db, root) = create_trie::<Layout>(TEST_DATA);

		let recorder = Recorder::default();

		for i in 0..2 {
			recorder.start_transaction();
			{
				let mut trie_recorder = recorder.as_trie_recorder(root);
				let trie = TrieDBBuilder::<Layout>::new(&db, &root)
					.with_recorder(&mut trie_recorder)
					.build();

				assert_eq!(TEST_DATA[i].1.to_vec(), trie.get(TEST_DATA[i].0).unwrap().unwrap());
			}
		}

		recorder.rollback_transaction().unwrap();

		for i in 2..4 {
			recorder.start_transaction();
			{
				let mut trie_recorder = recorder.as_trie_recorder(root);
				let trie = TrieDBBuilder::<Layout>::new(&db, &root)
					.with_recorder(&mut trie_recorder)
					.build();

				assert_eq!(TEST_DATA[i].1.to_vec(), trie.get(TEST_DATA[i].0).unwrap().unwrap());
			}
		}

		recorder.rollback_transaction().unwrap();

		assert_eq!(2, recorder.inner.lock().transactions.len());

		for _ in 0..2 {
			recorder.commit_transaction().unwrap();
		}

		assert_eq!(0, recorder.inner.lock().transactions.len());

		let storage_proof = recorder.to_storage_proof();
		let memory_db: MemoryDB = storage_proof.into_memory_db();

		// Check that we recorded the required data
		let trie = TrieDBBuilder::<Layout>::new(&memory_db, &root).build();

		// Check that the required data is still present.
		for i in 0..4 {
			if i % 2 == 0 {
				assert_eq!(TEST_DATA[i].1.to_vec(), trie.get(TEST_DATA[i].0).unwrap().unwrap());
			} else {
				assert!(trie.get(TEST_DATA[i].0).is_err());
			}
		}
	}

	#[test]
	fn recorder_transaction_accessed_keys_works() {
		let key = TEST_DATA[0].0;
		let (db, root) = create_trie::<Layout>(TEST_DATA);

		let recorder = Recorder::default();

		{
			let trie_recorder = recorder.as_trie_recorder(root);
			assert!(matches!(trie_recorder.trie_nodes_recorded_for_key(key), RecordedForKey::None));
		}

		recorder.start_transaction();
		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			assert_eq!(
				sp_core::Blake2Hasher::hash(TEST_DATA[0].1),
				trie.get_hash(TEST_DATA[0].0).unwrap().unwrap()
			);
			assert!(matches!(trie_recorder.trie_nodes_recorded_for_key(key), RecordedForKey::Hash));
		}

		recorder.start_transaction();
		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			assert_eq!(TEST_DATA[0].1.to_vec(), trie.get(TEST_DATA[0].0).unwrap().unwrap());
			assert!(matches!(
				trie_recorder.trie_nodes_recorded_for_key(key),
				RecordedForKey::Value,
			));
		}

		recorder.rollback_transaction().unwrap();
		{
			let trie_recorder = recorder.as_trie_recorder(root);
			assert!(matches!(trie_recorder.trie_nodes_recorded_for_key(key), RecordedForKey::Hash));
		}

		recorder.rollback_transaction().unwrap();
		{
			let trie_recorder = recorder.as_trie_recorder(root);
			assert!(matches!(trie_recorder.trie_nodes_recorded_for_key(key), RecordedForKey::None));
		}

		recorder.start_transaction();
		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			assert_eq!(TEST_DATA[0].1.to_vec(), trie.get(TEST_DATA[0].0).unwrap().unwrap());
			assert!(matches!(
				trie_recorder.trie_nodes_recorded_for_key(key),
				RecordedForKey::Value,
			));
		}

		recorder.start_transaction();
		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			assert_eq!(
				sp_core::Blake2Hasher::hash(TEST_DATA[0].1),
				trie.get_hash(TEST_DATA[0].0).unwrap().unwrap()
			);
			assert!(matches!(
				trie_recorder.trie_nodes_recorded_for_key(key),
				RecordedForKey::Value
			));
		}

		recorder.rollback_transaction().unwrap();
		{
			let trie_recorder = recorder.as_trie_recorder(root);
			assert!(matches!(
				trie_recorder.trie_nodes_recorded_for_key(key),
				RecordedForKey::Value
			));
		}

		recorder.rollback_transaction().unwrap();
		{
			let trie_recorder = recorder.as_trie_recorder(root);
			assert!(matches!(trie_recorder.trie_nodes_recorded_for_key(key), RecordedForKey::None));
		}
	}

	#[test]
	fn recorder_ignoring_nodes_works() {
		let (db, root) = create_trie::<Layout>(TEST_DATA);

		let recorder = Recorder::default();

		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			for (key, data) in TEST_DATA.iter().take(3) {
				assert_eq!(data.to_vec(), trie.get(&key).unwrap().unwrap());
			}
		}

		assert!(recorder.estimate_encoded_size() > 10);
		let mut ignored_nodes = IgnoredNodes::from_storage_proof::<sp_core::Blake2Hasher>(
			&recorder.drain_storage_proof(),
		);

		let recorder = Recorder::with_ignored_nodes(ignored_nodes.clone());

		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			for (key, data) in TEST_DATA {
				assert_eq!(data.to_vec(), trie.get(&key).unwrap().unwrap());
			}
		}

		assert!(recorder.estimate_encoded_size() > TEST_DATA[3].1.len());
		let ignored_nodes2 = IgnoredNodes::from_storage_proof::<sp_core::Blake2Hasher>(
			&recorder.drain_storage_proof(),
		);

		ignored_nodes.extend(ignored_nodes2);

		let recorder = Recorder::with_ignored_nodes(ignored_nodes);

		{
			let mut trie_recorder = recorder.as_trie_recorder(root);
			let trie = TrieDBBuilder::<Layout>::new(&db, &root)
				.with_recorder(&mut trie_recorder)
				.build();

			for (key, data) in TEST_DATA {
				assert_eq!(data.to_vec(), trie.get(&key).unwrap().unwrap());
			}
		}
		assert_eq!(0, recorder.estimate_encoded_size());
	}
}
