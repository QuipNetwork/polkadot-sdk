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

//! Reads a distinct persistent storage key each of `rounds` times.
//!
//! Under per-transaction cold/hot access pricing only the first (cold) touch of a key bills the
//! worst-case `proof_size`; repeats are hot and near-free. Using a fresh key every round keeps
//! every read cold, so the *metered* cost grows linearly with `rounds`, while the *true* PoV is a
//! handful of shared trie nodes — counted once by a proof recorder. That over-charge is what a
//! recorder reclaims; without one it accumulates, which is the revive trace-replay bug (the block
//! tail hits `ExhaustsResources` on recorder-less replay). `get_storage` is an explicit host
//! call, so the reads cannot be optimized away or hoisted out of the loop.

#![no_std]
#![no_main]
include!("../panic_handler.rs");

use uapi::{input, HostFn, HostFnImpl as api, StorageFlags};

const KEY: [u8; 32] = [1u8; 32];
// `limits::STORAGE_BYTES` (416) is the max storage value size in revive.
static mut BUFFER: [u8; 416] = [0u8; 416];

#[no_mangle]
#[polkavm_derive::polkavm_export]
pub extern "C" fn deploy() {
	// Seed a sizeable persistent value so each read pulls real trie data into the PoV.
	let data = unsafe { &BUFFER[..] };
	api::set_storage(StorageFlags::empty(), &KEY, data);
}

#[no_mangle]
#[polkavm_derive::polkavm_export]
pub extern "C" fn call() {
	input!(rounds: u32,);

	let mut key = KEY;
	for i in 0..rounds {
		key[..4].copy_from_slice(&i.to_le_bytes());
		let out = unsafe { &mut &mut BUFFER[..] };
		let _ = api::get_storage(StorageFlags::empty(), &key, out);
	}
}
