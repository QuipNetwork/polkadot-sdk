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

#![cfg_attr(substrate_runtime, no_std, no_main)]

#[cfg(substrate_runtime)]
mod fixture;

#[cfg(not(substrate_runtime))]
#[allow(dead_code)]
mod binary {
	include!(concat!(env!("OUT_DIR"), "/wasm_binary.rs"));
}

#[cfg(not(substrate_runtime))]
pub fn binary() -> &'static [u8] {
	let _ = binary::WASM_BINARY_BLOATY;
	binary::WASM_BINARY.unwrap()
}
