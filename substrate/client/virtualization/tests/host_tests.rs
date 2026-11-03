// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Native host-side tests exercising the [`VirtManager`] backend through the
//! virtualization host functions.

use sc_virtualization::VirtManager;
use sp_virtualization::{
	tests::{make_handler, run_loop, RunResult, GAS_MAX},
	CompileStatus, Module, ModuleError,
};

fn setup() -> sp_io::TestExternalities {
	sp_tracing::try_init_simple();
	let mut ext = sp_io::TestExternalities::default();
	ext.register_extension(sp_virtualization::VirtManagerExt::new(VirtManager::default()));
	ext
}

fn binary() -> &'static [u8] {
	sp_virtualization_test_fixture::binary()
}

/// Drives every wasm-callable test through the host implementation in one shot.
#[test]
fn run_all() {
	setup().execute_with(|| sp_virtualization::run_tests(binary()));
}

/// Compile with an identifier, then `from_storage_key` with that identifier hits the cache.
///
/// First compile is `Compiled`, the lookup-via-identifier is `Cached`.
#[test]
fn from_storage_key_cache_hit() {
	let program = binary();
	let key = b"some-cache-key";
	setup().execute_with(|| {
		let (_module, status) = Module::from_bytes(program, Some(key)).unwrap();
		assert_eq!(status, CompileStatus::Compiled);
		let (module, status) = Module::from_storage_key(key, b"").unwrap();
		assert_eq!(status, CompileStatus::Cached);
		let instance = module.instantiate().unwrap();
		let execution = instance.prepare(b"counter").unwrap();
		let mut gas_left = GAS_MAX;
		let mut counter: u64 = 0;
		let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
		assert!(matches!(result, RunResult::Ok(_)));
		assert_eq!(counter, 8);
	});
}

/// `Module::lookup` is a pure cache lookup: hits after a prior cached compile, misses otherwise,
/// and does not fall back to storage even when the identifier exists at a storage key.
#[test]
fn lookup_is_pure() {
	let program = binary();
	let key: &[u8] = b"stored-but-not-cached";

	let mut ext = setup();
	// Put the program at `key` in storage, but never compile under that identifier.
	ext.insert(key.to_vec(), program.to_vec());
	ext.execute_with(|| {
		// Lookup must NOT trigger a storage read.
		assert!(matches!(Module::lookup(key), Err(ModuleError::NotCached)));

		// After a cached compile, lookup hits.
		let other: &[u8] = b"some-other-key";
		let _ = Module::from_bytes(program, Some(other)).unwrap();
		assert!(Module::lookup(other).is_ok());
	});
}

/// `from_bytes` with `Some(identifier)` looks up the cache first; a second call with
/// the same identifier returns `Cached` without recompiling.
#[test]
fn from_bytes_cache_hit() {
	let program = binary();
	let key = b"compile-twice";
	setup().execute_with(|| {
		let (_module, status) = Module::from_bytes(program, Some(key)).unwrap();
		assert_eq!(status, CompileStatus::Compiled);
		let (_module, status) = Module::from_bytes(program, Some(key)).unwrap();
		assert_eq!(status, CompileStatus::Cached);
	});
}

/// `from_bytes` with `None` does not populate the cache.
#[test]
fn from_bytes_none_skips_cache() {
	let program = binary();
	let key = b"would-be-key";
	setup().execute_with(|| {
		let (_module, status) = Module::from_bytes(program, None).unwrap();
		assert_eq!(status, CompileStatus::Compiled);
		assert!(matches!(Module::from_storage_key(key, b""), Err(ModuleError::NotFound)));
	});
}

/// Load code from main trie storage on cache miss; the second call hits the cache.
#[test]
fn from_storage_key_main_trie() {
	let program = binary();
	let key: &[u8] = b"code:my-program";

	let mut ext = setup();
	ext.insert(key.to_vec(), program.to_vec());
	ext.execute_with(|| {
		let (module, status) = Module::from_storage_key(key, b"").unwrap();
		assert_eq!(status, CompileStatus::Compiled);
		let instance = module.instantiate().unwrap();
		let execution = instance.prepare(b"counter").unwrap();
		let mut gas_left = GAS_MAX;
		let mut counter: u64 = 0;
		let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
		assert!(matches!(result, RunResult::Ok(_)));
		assert_eq!(counter, 8);

		// Second call should hit the cache now.
		let (module, status) = Module::from_storage_key(key, b"").unwrap();
		assert_eq!(status, CompileStatus::Cached);
		let instance = module.instantiate().unwrap();
		let execution = instance.prepare(b"counter").unwrap();
		let mut counter: u64 = 0;
		let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
		assert!(matches!(result, RunResult::Ok(_)));
		assert_eq!(counter, 8);
	});
}

/// Load code from child trie storage on cache miss.
#[test]
fn from_storage_key_child_trie() {
	let program = binary();
	let key: &[u8] = b"code:my-program";
	let child_trie = b"contracts";
	let child_info = sp_storage::ChildInfo::new_default(child_trie);

	let mut ext = setup();
	ext.insert_child(child_info, key.to_vec(), program.to_vec());
	ext.execute_with(|| {
		let (module, status) = Module::from_storage_key(key, child_trie).unwrap();
		assert_eq!(status, CompileStatus::Compiled);
		let instance = module.instantiate().unwrap();
		let execution = instance.prepare(b"counter").unwrap();
		let mut gas_left = GAS_MAX;
		let mut counter: u64 = 0;
		let result = run_loop(execution, &mut gas_left, make_handler(&mut counter));
		assert!(matches!(result, RunResult::Ok(_)));
		assert_eq!(counter, 8);
	});
}

/// Code at the storage key is not a valid PolkaVM program.
#[test]
fn from_storage_key_invalid_image() {
	let key: &[u8] = b"code:garbage";

	let mut ext = setup();
	ext.insert(key.to_vec(), b"this is not a valid polkavm program".to_vec());
	ext.execute_with(|| {
		assert!(matches!(Module::from_storage_key(key, b""), Err(ModuleError::InvalidImage)));
	});
}
