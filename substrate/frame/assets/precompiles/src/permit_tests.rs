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

use super::permit;
use crate::mock::{new_test_ext, Test};
use pallet_revive::precompiles::H160;
use sp_core::{H256, U256};

// =============================================================================
// Test Helpers and Constants
// =============================================================================

/// Helper to create a verifying contract address for tests.
fn test_verifying_contract() -> H160 {
	H160::from_low_u64_be(0x1234)
}

/// Helper to create a test token name for EIP-712 domain separator.
fn test_token_name() -> &'static [u8] {
	b"Test Token"
}

/// Helper to create a future deadline (far in the future).
/// EIP-2612 specifies deadlines in UNIX seconds.
fn future_deadline() -> [u8; 32] {
	// Unix timestamp for year 2100 in seconds
	U256::from(4102444800u64).to_big_endian()
}

/// Helper to create a past deadline.
/// EIP-2612 specifies deadlines in UNIX seconds.
fn past_deadline() -> [u8; 32] {
	// Unix timestamp for year 2020 in seconds
	U256::from(1577836800u64).to_big_endian()
}

/// Hardhat account #0 address: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
///
/// This is a well-known test address derived from the private key:
/// 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
///
/// DO NOT use in production!
const HARDHAT_ACCOUNT_0: H160 = H160([
	0xf3, 0x9F, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xF6, 0xF4, 0xce, 0x6a, 0xB8, 0x82, 0x72, 0x79, 0xcf,
	0xfF, 0xb9, 0x22, 0x66,
]);

/// Parameters for a valid pre-computed permit signature.
///
/// Generated using Hardhat account #0 private key with these parameters:
/// - Chain ID: 31337
/// - Token Name: "Asset Permit"
/// - Owner: 0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266
/// - Verifying Contract: 0x0000000000000000000000000000000012345678
/// - Spender: 0x0000000000000000000000000000000098765432
/// - Value: 1000
/// - Nonce: 0
/// - Deadline: u64::MAX (18446744073709551615)
struct ValidPermitParams {
	verifying_contract: H160,
	name: &'static [u8],
	owner: H160,
	spender: H160,
	value: [u8; 32],
	deadline: [u8; 32],
	v: u8,
	r: [u8; 32],
	s: [u8; 32],
}

fn valid_permit_params() -> ValidPermitParams {
	ValidPermitParams {
		verifying_contract: H160::from_low_u64_be(0x1234_5678),
		name: b"Asset Permit",
		owner: HARDHAT_ACCOUNT_0,
		spender: H160::from_low_u64_be(0x9876_5432),
		value: U256::from(1000).to_big_endian(),
		deadline: U256::from(u64::MAX).to_big_endian(),
		v: 27u8,
		r: [
			175, 252, 243, 1, 254, 212, 189, 22, 49, 158, 63, 188, 243, 21, 56, 240, 124, 215, 220,
			121, 137, 153, 208, 70, 123, 109, 221, 94, 191, 131, 210, 111,
		],
		s: [
			21, 240, 201, 4, 59, 104, 154, 99, 230, 111, 29, 9, 150, 225, 57, 209, 15, 222, 27, 5,
			147, 40, 44, 246, 24, 108, 82, 129, 121, 73, 44, 234,
		],
	}
}

// =============================================================================
// Nonce Tests
// =============================================================================

#[test]
fn nonce_starts_at_zero() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let owner = H160::from_low_u64_be(1);

		let nonce = permit::Pallet::<Test>::nonce(&verifying_contract, &owner);
		assert_eq!(nonce, U256::zero());
	});
}

#[test]
fn nonce_increments() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let owner = H160::from_low_u64_be(1);

		let nonce1 = permit::Pallet::<Test>::increment_nonce(&verifying_contract, &owner).unwrap();
		assert_eq!(nonce1, U256::one());

		let nonce2 = permit::Pallet::<Test>::increment_nonce(&verifying_contract, &owner).unwrap();
		assert_eq!(nonce2, U256::from(2));

		let nonce_read = permit::Pallet::<Test>::nonce(&verifying_contract, &owner);
		assert_eq!(nonce_read, U256::from(2));
	});
}

#[test]
fn nonces_are_independent_per_verifying_contract() {
	new_test_ext().execute_with(|| {
		let owner = H160::from_low_u64_be(1);
		let contract_1 = H160::from_low_u64_be(0x1111);
		let contract_2 = H160::from_low_u64_be(0x2222);

		permit::Pallet::<Test>::increment_nonce(&contract_1, &owner).unwrap();
		permit::Pallet::<Test>::increment_nonce(&contract_1, &owner).unwrap();

		assert_eq!(permit::Pallet::<Test>::nonce(&contract_1, &owner), U256::from(2));
		assert_eq!(permit::Pallet::<Test>::nonce(&contract_2, &owner), U256::zero());
	});
}

#[test]
fn nonces_are_independent_per_owner() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let owner1 = H160::from_low_u64_be(1);
		let owner2 = H160::from_low_u64_be(2);

		permit::Pallet::<Test>::increment_nonce(&verifying_contract, &owner1).unwrap();
		permit::Pallet::<Test>::increment_nonce(&verifying_contract, &owner1).unwrap();

		assert_eq!(permit::Pallet::<Test>::nonce(&verifying_contract, &owner1), U256::from(2));
		assert_eq!(permit::Pallet::<Test>::nonce(&verifying_contract, &owner2), U256::zero());
	});
}

// =============================================================================
// Domain Separator Tests
// =============================================================================

#[test]
fn domain_separator_is_computed() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let name = test_token_name();
		let separator = permit::Pallet::<Test>::compute_domain_separator(&verifying_contract, name);
		// Should be a non-zero hash
		assert_ne!(separator, H256::zero());
	});
}

#[test]
fn domain_separator_is_deterministic() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let name = test_token_name();
		let separator1 =
			permit::Pallet::<Test>::compute_domain_separator(&verifying_contract, name);
		let separator2 =
			permit::Pallet::<Test>::compute_domain_separator(&verifying_contract, name);
		// Should return the same value for same inputs
		assert_eq!(separator1, separator2);
	});
}

#[test]
fn domain_separators_differ_per_verifying_contract() {
	new_test_ext().execute_with(|| {
		let contract_1 = H160::from_low_u64_be(0x1111);
		let contract_2 = H160::from_low_u64_be(0x2222);
		let name = test_token_name();

		let separator1 = permit::Pallet::<Test>::compute_domain_separator(&contract_1, name);
		let separator2 = permit::Pallet::<Test>::compute_domain_separator(&contract_2, name);

		// Domain separators should be different for different verifying contracts
		assert_ne!(separator1, separator2);
	});
}

#[test]
fn domain_separators_differ_per_token_name() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();

		let separator1 =
			permit::Pallet::<Test>::compute_domain_separator(&verifying_contract, b"Token A");
		let separator2 =
			permit::Pallet::<Test>::compute_domain_separator(&verifying_contract, b"Token B");

		// Domain separators should be different for different token names
		assert_ne!(separator1, separator2);
	});
}

// =============================================================================
// Permit Digest Tests
// =============================================================================

#[test]
fn permit_digest_is_deterministic() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let name = test_token_name();
		let owner = H160::from_low_u64_be(1);
		let spender = H160::from_low_u64_be(2);
		let value = [0u8; 32];
		let nonce = U256::zero();
		let deadline = [0u8; 32];

		let digest1 = permit::Pallet::<Test>::permit_digest(
			&verifying_contract,
			name,
			&owner,
			&spender,
			&value,
			&nonce,
			&deadline,
		);
		let digest2 = permit::Pallet::<Test>::permit_digest(
			&verifying_contract,
			name,
			&owner,
			&spender,
			&value,
			&nonce,
			&deadline,
		);

		assert_eq!(digest1, digest2);
	});
}

#[test]
fn permit_digest_changes_with_nonce() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let name = test_token_name();
		let owner = H160::from_low_u64_be(1);
		let spender = H160::from_low_u64_be(2);
		let value = [0u8; 32];
		let deadline = [0u8; 32];

		let digest1 = permit::Pallet::<Test>::permit_digest(
			&verifying_contract,
			name,
			&owner,
			&spender,
			&value,
			&U256::zero(),
			&deadline,
		);
		let digest2 = permit::Pallet::<Test>::permit_digest(
			&verifying_contract,
			name,
			&owner,
			&spender,
			&value,
			&U256::one(),
			&deadline,
		);

		assert_ne!(digest1, digest2);
	});
}

#[test]
fn permit_digest_changes_with_verifying_contract() {
	new_test_ext().execute_with(|| {
		let contract_1 = H160::from_low_u64_be(0x1111);
		let contract_2 = H160::from_low_u64_be(0x2222);
		let name = test_token_name();
		let owner = H160::from_low_u64_be(1);
		let spender = H160::from_low_u64_be(2);
		let value = [0u8; 32];
		let nonce = U256::zero();
		let deadline = [0u8; 32];

		let digest1 = permit::Pallet::<Test>::permit_digest(
			&contract_1,
			name,
			&owner,
			&spender,
			&value,
			&nonce,
			&deadline,
		);
		let digest2 = permit::Pallet::<Test>::permit_digest(
			&contract_2,
			name,
			&owner,
			&spender,
			&value,
			&nonce,
			&deadline,
		);

		// Digests should differ for different verifying contracts (domain separation)
		assert_ne!(digest1, digest2);
	});
}

// =============================================================================
// ECDSA Recovery Tests
// =============================================================================

#[test]
fn ecrecover_with_valid_signature() {
	new_test_ext().execute_with(|| {
		// Test vector generated with ethers.js:
		// const wallet = new
		// Wallet("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
		// const message = "test message";
		// const messageHash = ethers.keccak256(ethers.toUtf8Bytes(message));
		// const signature = wallet.signingKey.sign(messageHash);

		let message_hash = sp_io::hashing::keccak_256(b"test message");

		// Signature components from ethers.js signing
		let r: [u8; 32] = [
			0xbf, 0x50, 0xb8, 0x99, 0x85, 0xbd, 0x02, 0x4b, 0xd4, 0xf2, 0x5e, 0xa2, 0x1e, 0x72,
			0xe0, 0x56, 0xd4, 0x46, 0xdd, 0xe9, 0x8a, 0xac, 0x81, 0xf3, 0x10, 0x3c, 0x9e, 0x46,
			0x9e, 0x23, 0x1a, 0xad,
		];
		let s: [u8; 32] = [
			0x51, 0x91, 0x01, 0xf0, 0x2d, 0xaa, 0xbb, 0xd4, 0xaf, 0x51, 0xdf, 0x7f, 0xa2, 0x12,
			0xc1, 0x33, 0x88, 0xa9, 0x26, 0x10, 0x84, 0x2b, 0xda, 0xe8, 0x07, 0x26, 0x60, 0x99,
			0x36, 0x7c, 0xc6, 0x86,
		];
		let v = 27u8;

		let result = permit::Pallet::<Test>::ecrecover(&message_hash, v, &r, &s);

		// Should recover the correct address (Hardhat account #0)
		assert_eq!(result.unwrap(), HARDHAT_ACCOUNT_0);
	});
}

#[test]
fn ecrecover_fails_with_invalid_v() {
	new_test_ext().execute_with(|| {
		let digest = [0u8; 32];
		let r = [0u8; 32];
		let s = [0u8; 32];
		let v = 30u8; // Invalid v value (must be 27 or 28)

		let result = permit::Pallet::<Test>::ecrecover(&digest, v, &r, &s);
		assert!(matches!(result, Err(permit::pallet::Error::<Test>::InvalidVValue)));
	});
}

#[test]
fn ecrecover_fails_with_v_below_27() {
	new_test_ext().execute_with(|| {
		let digest = [0u8; 32];
		let r = [0u8; 32];
		let s = [0u8; 32];
		let v = 0u8; // Invalid v value

		let result = permit::Pallet::<Test>::ecrecover(&digest, v, &r, &s);
		assert!(matches!(result, Err(permit::pallet::Error::<Test>::InvalidVValue)));
	});
}

// =============================================================================
// Signature Malleability Tests
// =============================================================================

#[test]
fn ecrecover_rejects_high_s_value() {
	new_test_ext().execute_with(|| {
		let digest = [0u8; 32];
		let r = [0u8; 32];
		// s value greater than SECP256K1_N_DIV_2
		let s: [u8; 32] = [
			0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00,
		];
		let v = 27u8;

		let result = permit::Pallet::<Test>::ecrecover(&digest, v, &r, &s);
		assert!(matches!(result, Err(permit::pallet::Error::<Test>::SignatureSValueTooHigh)));
	});
}

#[test]
fn ecrecover_accepts_s_at_boundary() {
	new_test_ext().execute_with(|| {
		let digest = [0u8; 32];
		let r: [u8; 32] = [
			0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
			0x00, 0x00, 0x00, 0x00,
		];
		// s value exactly at SECP256K1_N_DIV_2 (should be valid)
		let s = permit::SECP256K1_N_DIV_2;
		let v = 27u8;

		// Should not fail with SignatureSValueTooHigh
		let result = permit::Pallet::<Test>::ecrecover(&digest, v, &r, &s);
		// The signature itself might be invalid, but it should not fail due to s being too high
		assert!(!matches!(result, Err(permit::pallet::Error::<Test>::SignatureSValueTooHigh)));
	});
}

// =============================================================================
// Deadline Validation Tests
// =============================================================================

#[test]
fn verify_permit_fails_with_expired_deadline() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let name = test_token_name();
		let owner = H160::from_low_u64_be(1);
		let spender = H160::from_low_u64_be(2);
		let value = [0u8; 32];
		let deadline = past_deadline(); // Deadline in the past
		let r = [0u8; 32];
		let s = [0u8; 32];
		let v = 27u8;

		let result = permit::Pallet::<Test>::verify_permit(
			&verifying_contract,
			name,
			&owner,
			&spender,
			&value,
			&deadline,
			v,
			&r,
			&s,
		);

		assert!(matches!(result, Err(permit::pallet::Error::<Test>::PermitExpired)));
	});
}

// =============================================================================
// Use Permit (Replay Attack Prevention) Tests
// =============================================================================

#[test]
fn verify_permit_does_not_increment_nonce() {
	new_test_ext().execute_with(|| {
		let verifying_contract = test_verifying_contract();
		let name = test_token_name();
		let owner = H160::from_low_u64_be(1);
		let spender = H160::from_low_u64_be(2);
		let value = [0u8; 32];
		let deadline = future_deadline();
		let r = [0u8; 32];
		let s = [0u8; 32];
		let v = 27u8;

		let initial_nonce = permit::Pallet::<Test>::nonce(&verifying_contract, &owner);

		// Call verify_permit multiple times
		for _ in 0..3 {
			let _ = permit::Pallet::<Test>::verify_permit(
				&verifying_contract,
				name,
				&owner,
				&spender,
				&value,
				&deadline,
				v,
				&r,
				&s,
			);
		}

		// Nonce should remain unchanged
		let final_nonce = permit::Pallet::<Test>::nonce(&verifying_contract, &owner);
		assert_eq!(initial_nonce, final_nonce, "verify_permit must not modify nonce");
	});
}

#[test]
fn use_permit_succeeds_with_valid_signature() {
	new_test_ext().execute_with(|| {
		let p = valid_permit_params();

		// Verify initial nonce is zero
		let initial_nonce = permit::Pallet::<Test>::nonce(&p.verifying_contract, &p.owner);
		assert_eq!(initial_nonce, U256::zero(), "initial nonce should be zero");

		// First use_permit should succeed
		let result = permit::Pallet::<Test>::use_permit(
			&p.verifying_contract,
			p.name,
			&p.owner,
			&p.spender,
			&p.value,
			&p.deadline,
			p.v,
			&p.r,
			&p.s,
		);
		assert!(result.is_ok(), "use_permit should succeed with valid signature");

		// Nonce should now be 1
		let nonce_after = permit::Pallet::<Test>::nonce(&p.verifying_contract, &p.owner);
		assert_eq!(nonce_after, U256::one(), "nonce should be incremented to 1 after use_permit");
	});
}

/// This is the critical EIP-2612 security property test.
///
/// It verifies that once a permit signature has been used successfully,
/// the same signature cannot be replayed to grant additional allowances.
/// This is the fundamental protection against permit replay attacks.
#[test]
fn use_permit_rejects_replay_of_consumed_permit() {
	new_test_ext().execute_with(|| {
		let p = valid_permit_params();

		// First use: should succeed
		let first_result = permit::Pallet::<Test>::use_permit(
			&p.verifying_contract,
			p.name,
			&p.owner,
			&p.spender,
			&p.value,
			&p.deadline,
			p.v,
			&p.r,
			&p.s,
		);
		assert!(first_result.is_ok(), "first use_permit should succeed");

		// Verify nonce was incremented
		let nonce = permit::Pallet::<Test>::nonce(&p.verifying_contract, &p.owner);
		assert_eq!(nonce, U256::one(), "nonce should be 1 after first use");

		// Replay attempt: should fail because nonce is now 1, but signature was for nonce 0
		let replay_result = permit::Pallet::<Test>::use_permit(
			&p.verifying_contract,
			p.name,
			&p.owner,
			&p.spender,
			&p.value,
			&p.deadline,
			p.v,
			&p.r,
			&p.s,
		);

		// The replay should fail with SignerMismatch because the digest computed
		// with nonce=1 won't match the signature created for nonce=0
		assert!(
			replay_result.is_err(),
			"replay of consumed permit MUST fail - this is a critical security property"
		);
		assert!(
			matches!(replay_result, Err(permit::pallet::Error::<Test>::SignerMismatch)),
			"replay should fail with SignerMismatch due to nonce mismatch in digest"
		);

		// Nonce should still be 1 (failed attempt should not increment)
		let nonce_after_replay = permit::Pallet::<Test>::nonce(&p.verifying_contract, &p.owner);
		assert_eq!(
			nonce_after_replay,
			U256::one(),
			"nonce should remain 1 after failed replay attempt"
		);
	});
}

/// Test that multiple consecutive replays all fail.
/// This ensures the protection isn't a one-time check.
#[test]
fn use_permit_rejects_multiple_replay_attempts() {
	new_test_ext().execute_with(|| {
		let p = valid_permit_params();

		// First use: should succeed
		let first_result = permit::Pallet::<Test>::use_permit(
			&p.verifying_contract,
			p.name,
			&p.owner,
			&p.spender,
			&p.value,
			&p.deadline,
			p.v,
			&p.r,
			&p.s,
		);
		assert!(first_result.is_ok(), "first use_permit should succeed");

		// Multiple replay attempts: all should fail
		for attempt in 1..=5 {
			let replay_result = permit::Pallet::<Test>::use_permit(
				&p.verifying_contract,
				p.name,
				&p.owner,
				&p.spender,
				&p.value,
				&p.deadline,
				p.v,
				&p.r,
				&p.s,
			);
			assert!(replay_result.is_err(), "replay attempt {} should fail", attempt);
		}

		// Nonce should still be 1 (no failed attempts should increment)
		let final_nonce = permit::Pallet::<Test>::nonce(&p.verifying_contract, &p.owner);
		assert_eq!(
			final_nonce,
			U256::one(),
			"nonce should remain 1 after all failed replay attempts"
		);
	});
}

// =============================================================================
// PERMIT_TYPEHASH Tests
// =============================================================================

#[test]
fn permit_typehash_is_correct() {
	let computed = sp_io::hashing::keccak_256(
		b"Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
	);
	assert_eq!(computed, permit::PERMIT_TYPEHASH);
}

// =============================================================================
// Constants Tests
// =============================================================================

#[test]
fn secp256k1_n_div_2_is_correct() {
	// n/2 should be 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0
	let expected = [
		0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
		0xFF, 0x5D, 0x57, 0x6E, 0x73, 0x57, 0xA4, 0x50, 0x1D, 0xDF, 0xE9, 0x2F, 0x46, 0x68, 0x1B,
		0x20, 0xA0,
	];
	assert_eq!(permit::SECP256K1_N_DIV_2, expected);
}

#[test]
fn encoded_length_constants_are_correct() {
	assert_eq!(permit::DOMAIN_SEPARATOR_ENCODED_LEN, 160);
	assert_eq!(permit::PERMIT_STRUCT_ENCODED_LEN, 192);
	assert_eq!(permit::DIGEST_PREFIX_LEN, 66);
}
