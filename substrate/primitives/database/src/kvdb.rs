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

/// A wrapper around `kvdb::Database` that implements `sp_database::Database` trait
use ::kvdb::{DBTransaction, KeyValueDB};
use std::collections::HashMap;
#[cfg(debug_assertions)]
use std::collections::HashSet;

use crate::{error, Change, ColumnId, Database, Transaction};

struct DbAdapter<D: KeyValueDB + 'static>(D);

fn handle_err<T>(result: std::io::Result<T>) -> T {
	match result {
		Ok(r) => r,
		Err(e) => {
			panic!("Critical database error: {:?}", e);
		},
	}
}

/// Read the reference counter for a key.
fn read_counter(
	db: &dyn KeyValueDB,
	col: ColumnId,
	key: &[u8],
) -> error::Result<(Vec<u8>, Option<u32>)> {
	let mut counter_key = key.to_vec();
	counter_key.push(0);
	Ok(match db.get(col, &counter_key).map_err(|e| error::DatabaseError(Box::new(e)))? {
		Some(data) => {
			let mut counter_data = [0; 4];
			if data.len() != 4 {
				return Err(error::DatabaseError(Box::new(std::io::Error::other(format!(
					"Unexpected counter len {}",
					data.len(),
				)))));
			}
			counter_data.copy_from_slice(&data);
			let counter = u32::from_le_bytes(counter_data);
			(counter_key, Some(counter))
		},
		None => (counter_key, None),
	})
}

enum RefCountedOp {
	Store(Vec<u8>),
	Reference,
	Release,
}

/// Commit a transaction to a KeyValueDB.
///
/// Ref-counted ops on the same `(col, key)` are replayed in order against one on-disk counter
/// read, then the final counter/value state is emitted. Without this, multiple
/// `Store`/`Reference`/`Release` in one tx would each read the stale on-disk counter and write
/// back to the same counter key — the underlying batch keeps only the last `put`, collapsing N
/// ops into one.
///
/// `Set`/`Remove` are emitted in submission order; ref-counted ops are emitted afterwards.
/// Debug builds assert that raw and ref-counted ops are not mixed on the same `(col, key)`.
fn commit_impl<H: Clone + AsRef<[u8]>>(
	db: &dyn KeyValueDB,
	transaction: Transaction<H>,
) -> error::Result<()> {
	let mut tx = DBTransaction::new();
	let mut ref_counted: HashMap<(ColumnId, Vec<u8>), Vec<RefCountedOp>> = HashMap::new();
	#[cfg(debug_assertions)]
	let mut raw_keys: HashSet<(ColumnId, Vec<u8>)> = HashSet::new();

	for change in transaction.0.into_iter() {
		match change {
			Change::Set(col, key, value) => {
				#[cfg(debug_assertions)]
				raw_keys.insert((col, key.clone()));
				tx.put_vec(col, &key, value);
			},
			Change::Remove(col, key) => {
				#[cfg(debug_assertions)]
				raw_keys.insert((col, key.clone()));
				tx.delete(col, &key);
			},
			Change::Store(col, key, value) => {
				ref_counted
					.entry((col, key.as_ref().to_vec()))
					.or_default()
					.push(RefCountedOp::Store(value));
			},
			Change::Reference(col, key) => {
				ref_counted
					.entry((col, key.as_ref().to_vec()))
					.or_default()
					.push(RefCountedOp::Reference);
			},
			Change::Release(col, key) => {
				ref_counted
					.entry((col, key.as_ref().to_vec()))
					.or_default()
					.push(RefCountedOp::Release);
			},
		}
	}

	#[cfg(debug_assertions)]
	for raw_key in &raw_keys {
		debug_assert!(
			!ref_counted.contains_key(raw_key),
			"mixed raw/ref-counted database ops on column {}, key {:02x?}",
			raw_key.0,
			raw_key.1,
		);
	}

	for ((col, key), ops) in ref_counted {
		let (counter_key, mut counter) = read_counter(db, col, &key)?;

		let mut value_to_write = None;
		for op in ops {
			match op {
				RefCountedOp::Store(value) => match counter {
					Some(c) => counter = Some(c + 1),
					None => {
						counter = Some(1);
						value_to_write = Some(value);
					},
				},
				RefCountedOp::Reference => {
					if let Some(c) = counter {
						counter = Some(c + 1);
					}
				},
				RefCountedOp::Release => match counter {
					Some(1) => {
						counter = None;
						value_to_write = None;
					},
					Some(c) => counter = Some(c - 1),
					None => {},
				},
			}
		}

		match counter {
			Some(counter) => {
				tx.put(col, &counter_key, &counter.to_le_bytes());
				if let Some(value) = value_to_write {
					tx.put_vec(col, &key, value);
				}
			},
			None => {
				tx.delete(col, &counter_key);
				tx.delete(col, &key);
			},
		}
	}

	db.write(tx).map_err(|e| error::DatabaseError(Box::new(e)))
}

/// Wrap generic kvdb-based database into a trait object that implements [`Database`].
pub fn as_database<D, H>(db: D) -> std::sync::Arc<dyn Database<H>>
where
	D: KeyValueDB + 'static,
	H: Clone + AsRef<[u8]>,
{
	std::sync::Arc::new(DbAdapter(db))
}

impl<D: KeyValueDB, H: Clone + AsRef<[u8]>> Database<H> for DbAdapter<D> {
	fn commit(&self, transaction: Transaction<H>) -> error::Result<()> {
		commit_impl(&self.0, transaction)
	}

	fn get(&self, col: ColumnId, key: &[u8]) -> Option<Vec<u8>> {
		handle_err(self.0.get(col, key))
	}

	fn contains(&self, col: ColumnId, key: &[u8]) -> bool {
		handle_err(self.0.has_key(col, key))
	}
}

/// RocksDB-specific adapter that implements `optimize_db` via `force_compact`.
#[cfg(feature = "rocksdb")]
pub struct RocksDbAdapter(kvdb_rocksdb::Database);

#[cfg(feature = "rocksdb")]
impl<H: Clone + AsRef<[u8]>> Database<H> for RocksDbAdapter {
	fn commit(&self, transaction: Transaction<H>) -> error::Result<()> {
		commit_impl(&self.0, transaction)
	}

	fn get(&self, col: ColumnId, key: &[u8]) -> Option<Vec<u8>> {
		handle_err(self.0.get(col, key))
	}

	fn contains(&self, col: ColumnId, key: &[u8]) -> bool {
		handle_err(self.0.has_key(col, key))
	}

	fn optimize_db_col(&self, col: ColumnId) -> error::Result<()> {
		self.0.force_compact(col).map_err(|e| error::DatabaseError(Box::new(e)))
	}
}

/// Wrap RocksDB database into a trait object with `optimize_db` support.
#[cfg(feature = "rocksdb")]
pub fn as_rocksdb_database<H>(db: kvdb_rocksdb::Database) -> std::sync::Arc<dyn Database<H>>
where
	H: Clone + AsRef<[u8]>,
{
	std::sync::Arc::new(RocksDbAdapter(db))
}
