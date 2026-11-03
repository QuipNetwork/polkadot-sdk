// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use it except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::shared::new_rng;
use log::warn;
use rand::prelude::*;
use sc_cli::{Error, Result};

pub(crate) type EmptyStorage = Error;

/// Returns a shuffled list of entries and an RNG. Behavior:
///
/// - If `keys_limit` is `None`, collects all entries from `iter_from_start`.
/// - Otherwise computes `first_key = blake2_256(random_seed.to_be_bytes())` and takes up to
///   `keys_limit` entries from `iter_from_first_key(Some(first_key))` (backend uses exclusive
///   start, so entries strictly after `first_key`). If that yields fewer than `keys_limit`, circles
///   back: takes entries from `iter_from_start()` with `key_of(entry) < first_key` until the limit
///   is reached or no more such entries exist.
///
/// The three closures abstract the different ways to read entries:
/// - **Keys only** (warmup, read): the two iterators use `client.storage_keys(..., start_at)`;
///   `key_of` returns the key bytes (e.g. `|k: &StorageKey| k.0.as_slice()`).
/// - **Pairs** (write): the two iterators use `trie.pairs(iter_args)`; `key_of` returns the key
///   part of each pair (e.g. for `Result<(Vec<u8>, Vec<u8>), _>` use the first element).
pub(crate) fn select_entries<E, I1, I2, F1, F2, FKey>(
	keys_limit: Option<usize>,
	random_seed: Option<u64>,
	iter_from_first_key: F1,
	iter_from_start: F2,
	key_of: FKey,
) -> Result<(Vec<E>, impl Rng)>
where
	I1: Iterator<Item = E>,
	I2: Iterator<Item = E>,
	F1: FnOnce(Option<&[u8]>) -> Result<I1>,
	F2: FnOnce() -> Result<I2>,
	FKey: Fn(&E) -> &[u8],
{
	let mut entries: Vec<E> = if let Some(limit) = keys_limit {
		let first_key =
			random_seed.map(|seed| sp_crypto_hashing::blake2_256(&seed.to_be_bytes()[..]).to_vec());
		let from_first = iter_from_first_key(first_key.as_deref())?;
		let mut collected: Vec<E> = from_first.take(limit).collect();
		if collected.len() < limit {
			let need_more = limit - collected.len();
			if let Some(ref fk) = first_key {
				let extra: Vec<E> = iter_from_start()?
					.take_while(|e| key_of(e) < fk.as_slice())
					.take(need_more)
					.collect();
				collected.extend(extra);
			}
			if collected.len() < limit {
				warn!("Only {} entries available (requested {})", collected.len(), limit);
			}
		}
		collected
	} else {
		iter_from_start()?.collect()
	};

	if entries.is_empty() {
		return Err(EmptyStorage::Input("Can't process benchmarking with empty storage".into()));
	}

	let (mut rng, _) = new_rng(random_seed);
	entries.shuffle(&mut rng);

	Ok((entries, rng))
}
