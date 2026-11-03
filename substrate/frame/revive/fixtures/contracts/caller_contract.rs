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

#![no_std]
#![no_main]
include!("../panic_handler.rs");

use uapi::{input, u256_bytes, HostFn, HostFnImpl as api, ReturnErrorCode};

const INPUT: [u8; 8] = [0u8, 1, 34, 51, 68, 85, 102, 119];
const REVERTED_INPUT: [u8; 7] = [1u8, 34, 51, 68, 85, 102, 119];

#[no_mangle]
#[polkavm_derive::polkavm_export]
pub extern "C" fn deploy() {}

#[no_mangle]
#[polkavm_derive::polkavm_export]
pub extern "C" fn call() {
	input!(code_hash: &[u8; 32], load_code_ref_time: u64, load_code_proof_size: u64,);

	// The value to transfer on instantiation and calls. Chosen to be greater than existential
	// deposit.
	let value = u256_bytes(32_768_000_000u64);
	let salt = [0u8; 32];

	// Callee will use the first 4 bytes of the input to return an exit status.
	let mut input_deploy = [0; 32 + INPUT.len()];
	input_deploy[..32].copy_from_slice(code_hash);
	input_deploy[32..].copy_from_slice(&INPUT);

	let mut reverted_input_deploy = [0; 32 + REVERTED_INPUT.len()];
	reverted_input_deploy[..32].copy_from_slice(code_hash);
	reverted_input_deploy[32..].copy_from_slice(&REVERTED_INPUT);

	// Fail to deploy the contract since it returns a non-zero exit status.
	let res = api::instantiate(
		u64::MAX,       /* How much ref_time weight to devote for the execution. u64::MAX = use
		                 * all. */
		u64::MAX, // How much proof_size weight to devote for the execution. u64::MAX = use all.
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&reverted_input_deploy,
		None,
		None,
		Some(&salt),
	);
	assert!(matches!(res, Err(ReturnErrorCode::CalleeReverted)));

	// Fail to deploy the contract due to insufficient ref_time weight.
	let res = api::instantiate(
		1u64, // too little ref_time weight
		u64::MAX, /* How much proof_size weight to devote for the execution. u64::MAX =
		       * use all. */
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&input_deploy,
		None,
		None,
		Some(&salt),
	);
	assert!(matches!(res, Err(ReturnErrorCode::OutOfResources)));

	// Fail to deploy the contract due to insufficient proof_size weight.
	let res = api::instantiate(
		u64::MAX,       /* How much ref_time weight to devote for the execution. u64::MAX = use
		                 * all. */
		1u64,           // Too little proof_size weight
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&input_deploy,
		None,
		None,
		Some(&salt),
	);
	assert!(matches!(res, Err(ReturnErrorCode::OutOfResources)));

	// Fail to deploy because the body runs out of proof_size after code load.
	// Done before the successful deploy so the storage slot touched inside this entrypoint's body
	// is still cold; once a successful deploy commits, the slot is hot and `clear_storage` no
	// longer OOGs.
	let mut deploy_revert_output = [0u8; 4];
	let res = api::instantiate(
		u64::MAX, // How much ref_time weight to devote for the execution. u64::MAX = use all.
		load_code_proof_size, // just enough to load the contract
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&input_deploy,
		None,
		Some(&mut &mut deploy_revert_output[..]),
		Some(&salt),
	);
	assert!(matches!(res, Err(ReturnErrorCode::CalleeReverted)));

	let mut decode_buf = [0u8; 4];
	decode_buf[..4].copy_from_slice(&deploy_revert_output[..4]);
	assert_eq!(u32::from_le_bytes(decode_buf), ReturnErrorCode::OutOfResources as u32);

	// Deploy the contract successfully.
	let mut callee = [0u8; 20];

	api::instantiate(
		u64::MAX,       /* How much ref_time weight to devote for the execution. u64::MAX = use
		                 * all. */
		u64::MAX, // How much proof_size weight to devote for the execution. u64::MAX = use all.
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&input_deploy,
		Some(&mut callee),
		None,
		Some(&salt),
	)
	.unwrap();

	// Call the new contract and expect it to return failing exit code.
	let res = api::call(
		uapi::CallFlags::empty(),
		&callee,
		u64::MAX, // How much ref_time weight to devote for the execution. u64::MAX = use all.
		u64::MAX, // How much proof_size weight to devote for the execution. u64::MAX = use all.
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&REVERTED_INPUT,
		None,
	);
	assert!(matches!(res, Err(ReturnErrorCode::CalleeReverted)));

	// Fail to call the contract due to insufficient ref_time weight.
	let res = api::call(
		uapi::CallFlags::empty(),
		&callee,
		load_code_ref_time,   // just enough to load the contract
		load_code_proof_size, // just enough to load the contract
		&[u8::MAX; 32],       // No deposit limit.
		&value,
		&INPUT,
		None,
	);
	assert!(matches!(res, Err(ReturnErrorCode::OutOfResources)));

	// Call the contract successfully.
	let mut output = [0u8; 4];
	api::call(
		uapi::CallFlags::empty(),
		&callee,
		u64::MAX, // How much ref_time weight to devote for the execution. u64::MAX = use all.
		u64::MAX, // How much proof_size weight to devote for the execution. u64::MAX = use all.
		&[u8::MAX; 32], // No deposit limit.
		&value,
		&INPUT,
		Some(&mut &mut output[..]),
	)
	.unwrap();
	assert_eq!(&output, &INPUT[4..])
}
