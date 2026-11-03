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

//! Permit precompile integration tests.
//!
//! `permit_tests.rs` exercises `permit::Pallet` directly in isolation. The
//! tests in this file drive the same logic end-to-end through the precompile
//! dispatcher via `bare_call`, signing each digest at runtime with Hardhat
//! account #0's private key. They cover precompile-level concerns the
//! pallet tests cannot:
//!
//!   * the allowance-update branches in `permit()` (fresh-approve, revoke, noop,
//!     cancel-then-approve), each pinned by a dedicated test
//!   * `with_transaction` rollback (nonce, allowance, deposit, contract events)
//!   * Approval event emission
//!   * the dispatcher's revert-reason mapping
//!   * cross-prefix (verifying-contract) domain separation

use crate::{
	alloy::hex,
	mock::{new_test_ext, Assets, Balances, RuntimeEvent, RuntimeOrigin, System, Test},
	permit,
	test_helpers::{
		assert_contract_event, set_prefix_in_address, setup_asset_for_prefix, ICaller,
		PRECOMPILE_ADDRESS_PREFIX, PRECOMPILE_ADDRESS_PREFIX_FOREIGN,
	},
	IERC20::{self, IERC20Events},
};
use alloy::primitives::U256 as AlloyU256;
use frame_support::{
	assert_ok,
	traits::{Currency, Get},
};
use pallet_revive::{
	precompiles::{alloy, alloy::sol_types::SolCall, TransactionLimits, H160},
	AddressMapper, Code, ExecConfig,
};
use sp_core::U256;
use sp_runtime::Weight;
use test_case::test_case;

/// Hardhat account #0 address: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266.
///
/// Mirror of the constant in `permit_tests.rs`; both files sign with the
/// same well-known private key. DO NOT use in production.
const HARDHAT_ACCOUNT_0: H160 = H160([
	0xf3, 0x9F, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xF6, 0xF4, 0xce, 0x6a, 0xB8, 0x82, 0x72, 0x79, 0xcf,
	0xfF, 0xb9, 0x22, 0x66,
]);

const HARDHAT_ACCOUNT_0_SEED: &[u8] =
	b"0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

const PERMIT_TOKEN_NAME: &[u8] = b"Test Token";
/// 2100-01-01 00:00 UTC in seconds — used as a deadline that will not
/// expire during the lifetime of the test runtime.
const FAR_FUTURE_DEADLINE: u64 = 4_102_444_800;
/// Account id used as the EIP-2612 spender across permit tests. Arbitrary
/// non-zero u64 — the value is irrelevant as long as it differs from the
/// signer (Hardhat #0) and the relayer.
const SPENDER_ACCOUNT: u64 = 987_654_321;
/// Account id used as the relayer that submits the permit on behalf of
/// the signer. Arbitrary non-zero u64.
const SUBMITTER_ACCOUNT: u64 = 555;
/// Free balance given to a funded account — large enough to cover the
/// permit-call storage deposits and the `Caller` fixture's contract
/// deposit in the STATICCALL test.
const SUBMITTER_FUNDING: u128 = 1_000_000_000_000;
/// Account id used to deploy the `Caller` fixture contract in the
/// STATICCALL test. Distinct from the relayer / signer / spender so a
/// regression that crosses roles is visible.
const DEPLOYER_ACCOUNT: u64 = 1234;

/// The `u64` AccountId that the runtime's `AddressMapper` derives from
/// `HARDHAT_ACCOUNT_0`. Derived via the trait so this stays correct if
/// the mapper's derivation ever changes.
fn hardhat_account_id() -> u64 {
	<Test as pallet_revive::Config>::AddressMapper::to_account_id(&HARDHAT_ACCOUNT_0)
}

/// Sign an EIP-2612 permit digest with Hardhat #0's key. Reads the
/// current on-chain nonce so the digest is valid for an immediate
/// `permit()` call. Returns `(v, r, s)` in Ethereum format
/// (v ∈ {27, 28}).
fn sign_permit(
	asset_addr: H160,
	spender: H160,
	value: AlloyU256,
	deadline: AlloyU256,
) -> (u8, [u8; 32], [u8; 32]) {
	let nonce = permit::Pallet::<Test>::nonce(&asset_addr, &HARDHAT_ACCOUNT_0);
	let value_bytes: [u8; 32] = value.to_be_bytes();
	let deadline_bytes: [u8; 32] = deadline.to_be_bytes();

	let digest = permit::Pallet::<Test>::permit_digest(
		&asset_addr,
		PERMIT_TOKEN_NAME,
		&HARDHAT_ACCOUNT_0,
		&spender,
		&value_bytes,
		&nonce,
		&deadline_bytes,
	);

	// Sign via the keystore — works in both native and WASM, mirroring
	// the approach used in benchmarking.rs.
	let key_type = sp_core::crypto::KeyTypeId(*b"prmt");
	let pub_key = sp_io::crypto::ecdsa_generate(key_type, Some(HARDHAT_ACCOUNT_0_SEED.to_vec()));
	let sig = sp_io::crypto::ecdsa_sign_prehashed(key_type, &pub_key, &digest)
		.expect("signing with Hardhat #0 must succeed; qed");
	let sig_bytes: &[u8; 65] = sig.as_ref();
	let r: [u8; 32] = sig_bytes[0..32].try_into().expect("r is 32 bytes");
	let s: [u8; 32] = sig_bytes[32..64].try_into().expect("s is 32 bytes");
	let v: u8 = sig_bytes[64] + 27;
	(v, r, s)
}

/// Configures an asset owned by Hardhat #0 with metadata name
/// [`PERMIT_TOKEN_NAME`], returning the asset's precompile address.
/// Hardhat #0 is set as the asset admin so freeze tests can drive
/// `freeze_asset` from that account.
fn setup_permit_asset(asset_id: u32, prefix: u16) -> H160 {
	let asset_addr = H160::from(set_prefix_in_address(prefix));
	let owner = hardhat_account_id();
	Balances::make_free_balance_be(&owner, 1_000);
	setup_asset_for_prefix(asset_id, prefix);
	assert_ok!(Assets::force_create(RuntimeOrigin::root(), asset_id, owner, true, 1));
	assert_ok!(Assets::force_set_metadata(
		RuntimeOrigin::root(),
		asset_id,
		PERMIT_TOKEN_NAME.to_vec(),
		b"TST".to_vec(),
		18,
		false,
	));
	assert_ok!(Assets::mint(RuntimeOrigin::signed(owner), asset_id, owner, 100));
	asset_addr
}

/// Submits a `permit()` call via the precompile, returning the bare-call
/// result so callers can distinguish revert paths.
fn raw_permit(
	sender: u64,
	asset_addr: H160,
	owner: H160,
	spender: H160,
	value: AlloyU256,
	deadline: AlloyU256,
	v: u8,
	r: [u8; 32],
	s: [u8; 32],
) -> pallet_revive::ContractResult<pallet_revive::ExecReturnValue, u128> {
	let data = IERC20::permitCall {
		owner: owner.0.into(),
		spender: spender.0.into(),
		value,
		deadline,
		v,
		r: r.into(),
		s: s.into(),
	}
	.abi_encode();
	pallet_revive::Pallet::<Test>::bare_call(
		RuntimeOrigin::signed(sender),
		asset_addr,
		0u32.into(),
		TransactionLimits::WeightAndDeposit { weight_limit: Weight::MAX, deposit_limit: u128::MAX },
		data,
		&ExecConfig::new_substrate_tx(),
	)
}

/// Signs the current-nonce permit and submits it, asserting success.
fn permit_sign_and_call(
	submitter: u64,
	asset_addr: H160,
	spender: H160,
	value: AlloyU256,
	deadline: AlloyU256,
) {
	let (v, r, s) = sign_permit(asset_addr, spender, value, deadline);
	let result =
		raw_permit(submitter, asset_addr, HARDHAT_ACCOUNT_0, spender, value, deadline, v, r, s);
	assert!(result.result.is_ok(), "permit precompile call failed: {:?}", result);
	assert!(!result.result.unwrap().did_revert(), "permit call reverted");
}

/// Asserts a permit submission trapped with `Err(DispatchError::Module(_))`
/// matching the given pallet error variant. Use for the
/// `Error::Error(DispatchError)` trap path; for clean reverts use
/// `assert_permit_reverted_with`.
///
/// Strict equality against the lifted `DispatchError` ensures unrelated
/// failure modes (out-of-gas, panics, weight exhaustion, a different
/// pallet error) cannot silently keep the test green if the failure
/// surface changes.
fn assert_permit_dispatch_err<E>(
	result: pallet_revive::ContractResult<pallet_revive::ExecReturnValue, u128>,
	expected: E,
) where
	E: Into<sp_runtime::DispatchError>,
{
	use sp_runtime::DispatchError;
	let expected: DispatchError = expected.into();
	let actual = match result.result {
		Err(e) => e,
		Ok(v) => {
			panic!("permit expected to trap with {:?}; call returned Ok({:?})", expected, v)
		},
	};
	assert!(
		matches!(actual, DispatchError::Module(_)),
		"expected DispatchError::Module(...), got {:?}",
		actual,
	);
	assert_eq!(actual, expected);
}

/// Asserts the call cleanly reverted (not trapped) and that the revert
/// reason contains `expected_substring`.
///
/// **Avoid prefix collisions** — pass the *full* reason string. For
/// example, `"Invalid signature"` is a prefix of `"Invalid signature v
/// value"`, and matching the bare prefix would silently accept either.
fn assert_permit_reverted_with(
	result: pallet_revive::ContractResult<pallet_revive::ExecReturnValue, u128>,
	expected_substring: &str,
) {
	let exec = match result.result.as_ref() {
		Ok(v) => v,
		Err(e) => panic!(
			"expected revert with reason {:?}, got dispatch error: {:?}",
			expected_substring, e
		),
	};
	assert!(
		exec.did_revert(),
		"expected revert with reason {:?}, but call succeeded: {:?}",
		expected_substring,
		exec,
	);
	let needle = expected_substring.as_bytes();
	assert!(
		exec.data.windows(needle.len()).any(|w| w == needle),
		"expected revert reason to contain {:?}, got 0x{}",
		expected_substring,
		hex::encode(&exec.data),
	);
}

/// Asserts no `ContractEmitted` event was raised by `contract`. Used to
/// verify event rollback when a permit fails inside `with_transaction`.
fn assert_no_contract_event_from(contract: H160) {
	let any = System::events().iter().any(|er| {
		matches!(
			&er.event,
			RuntimeEvent::Revive(pallet_revive::Event::ContractEmitted { contract: c, .. }) if *c == contract,
		)
	});
	assert!(!any, "expected no ContractEmitted events from {:?}", contract);
}

fn fund_submitter(account: u64) {
	Balances::make_free_balance_be(&account, SUBMITTER_FUNDING);
}

/// Common setup shared by most permit tests: an asset registered behind
/// the given precompile prefix, the signer (Hardhat #0) as owner, a
/// fixed spender account/address, a funded relayer (`submitter`), and a
/// far-future deadline. Tests that need a different shape (e.g.
/// zero-address callers, or cross-prefix signing) build their state
/// directly.
struct PermitSetup {
	asset_id: u32,
	asset_addr: H160,
	owner_account: u64,
	spender_account: u64,
	spender_addr: H160,
	submitter: u64,
	deadline: AlloyU256,
}

fn permit_setup(prefix: u16) -> PermitSetup {
	let asset_id = 0u32;
	let asset_addr = setup_permit_asset(asset_id, prefix);
	let owner_account = hardhat_account_id();
	let spender_account = SPENDER_ACCOUNT;
	let spender_addr = <Test as pallet_revive::Config>::AddressMapper::to_address(&spender_account);
	let submitter = SUBMITTER_ACCOUNT;
	fund_submitter(submitter);
	let deadline = AlloyU256::from(FAR_FUTURE_DEADLINE);
	PermitSetup {
		asset_id,
		asset_addr,
		owner_account,
		spender_account,
		spender_addr,
		submitter,
		deadline,
	}
}

/// Drives `permit()` through the fresh-approve and revoke branches:
/// 0→100 (fresh), 100→0 (revoke), 0→50 (fresh again). Verifies
/// allowance, deposit, nonce, and the Approval event at each step. The
/// headline permit integration test — kept parametrized over both
/// prefixes for confidence on the cross-prefix asset_id extraction path.
/// The non-zero→non-zero branch is covered by `permit_nonzero_to_nonzero`.
#[test_case(PRECOMPILE_ADDRESS_PREFIX)]
#[test_case(PRECOMPILE_ADDRESS_PREFIX_FOREIGN)]
fn permit_set_and_revoke(asset_index: u16) {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(asset_index);
		let deposit: u128 = <Test as pallet_assets::Config>::ApprovalDeposit::get();

		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero()
		);

		// 0 → 100: fresh approval.
		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one()
		);
		assert_contract_event(
			setup.asset_addr,
			IERC20Events::Approval(IERC20::Approval {
				owner: HARDHAT_ACCOUNT_0.0.into(),
				spender: setup.spender_addr.0.into(),
				value: AlloyU256::from(100),
			}),
		);

		// 100 → 0: revoke. ERC-20 conformance: must fire Approval(_, _, 0).
		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(0),
			setup.deadline,
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			0
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), 0);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::from(2)
		);
		assert_contract_event(
			setup.asset_addr,
			IERC20Events::Approval(IERC20::Approval {
				owner: HARDHAT_ACCOUNT_0.0.into(),
				spender: setup.spender_addr.0.into(),
				value: AlloyU256::from(0),
			}),
		);

		// 0 → 50: fresh approval again.
		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(50),
			setup.deadline,
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			50
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::from(3)
		);
		assert_contract_event(
			setup.asset_addr,
			IERC20Events::Approval(IERC20::Approval {
				owner: HARDHAT_ACCOUNT_0.0.into(),
				spender: setup.spender_addr.0.into(),
				value: AlloyU256::from(50),
			}),
		);
	});
}

/// `permit(value=0)` against a non-existent allowance succeeds silently —
/// no allowance entry, no deposit, but the nonce IS consumed and an
/// `Approval(_, _, 0)` event IS emitted (matches ERC-20 set semantics).
/// Pins the `new_amount.is_zero() && current.is_zero()` noop branch in
/// the `permit` dispatcher.
#[test]
fn permit_zero_on_nonexistent_is_noop() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(0),
			setup.deadline,
		);

		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			0
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), 0);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one()
		);
		assert_contract_event(
			setup.asset_addr,
			IERC20Events::Approval(IERC20::Approval {
				owner: HARDHAT_ACCOUNT_0.0.into(),
				spender: setup.spender_addr.0.into(),
				value: AlloyU256::from(0),
			}),
		);
	});
}

/// Overwriting a non-zero allowance via permit must use set semantics:
/// the allowance equals the new value (not the sum), and only one
/// deposit is held throughout. Pins the cancel-then-approve branch in
/// the `permit` dispatcher (the `!new_amount.is_zero() && !current.is_zero()` arm).
#[test]
fn permit_nonzero_to_nonzero() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);
		let deposit: u128 = <Test as pallet_assets::Config>::ApprovalDeposit::get();

		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);

		// 100 → 50, no zeroing in between.
		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(50),
			setup.deadline,
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			50
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);

		// 50 → 200: confirm both directions.
		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(200),
			setup.deadline,
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			200
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);
	});
}

/// Replay of a consumed permit must fail through the precompile path.
/// EIP-2612's headline guarantee — `permit()` atomically verifies and
/// consumes a signature — is realized by `use_permit` incrementing the
/// nonce. A regression that swapped `use_permit` for `verify_permit`
/// (which does NOT bump the nonce) would pass every other test in this
/// submodule. Pinning the invariant at the precompile layer.
///
/// First submission consumes the permit (nonce 0 → 1). The same
/// `(v, r, s)` is then re-submitted; the precompile re-derives the
/// digest using the new on-chain nonce, recovery yields a different
/// signer, and `recovered != owner` fires "Signer does not match
/// owner".
#[test]
fn permit_replay_through_precompile_is_rejected() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, AlloyU256::from(100), setup.deadline);

		let first = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			v,
			r,
			s,
		);
		assert!(first.result.is_ok(), "first permit must succeed: {:?}", first);
		assert!(!first.result.expect("checked above").did_revert(), "first permit must not revert",);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one(),
			"first permit must advance the nonce",
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100,
		);

		// Replay the exact same (v, r, s).
		let replay = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			v,
			r,
			s,
		);
		assert_permit_reverted_with(replay, "Signer does not match owner");
		// Nonce must stay at 1 — the failure path must surface before
		// any further increment, and any half-applied state rolls back.
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one(),
			"failed replay must not advance the nonce past 1",
		);
		// Allowance must still reflect only the first submission.
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100,
		);
	});
}

/// If the inner allowance update fails after `use_permit` succeeded, the
/// whole storage transaction must roll back — nonce, allowance, deposit,
/// and (importantly) any contract event from the closure body. We
/// trigger the inner failure by freezing the asset after signing.
///
/// The `assert_no_contract_event_from` here also implicitly pins that
/// `pallet_revive`'s contract events ARE rolled back by
/// `frame_support::storage::with_transaction`.
#[test]
fn permit_rollback_does_not_increment_nonce() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, AlloyU256::from(100), setup.deadline);

		assert_ok!(Assets::freeze_asset(
			RuntimeOrigin::signed(setup.owner_account),
			setup.asset_id
		));

		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			v,
			r,
			s,
		);
		assert_permit_dispatch_err(result, pallet_assets::Error::<Test>::AssetNotLive);

		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero(),
			"nonce must remain 0 when the storage transaction rolls back"
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			0
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), 0);
		assert_no_contract_event_from(setup.asset_addr);
	});
}

/// A failed permit must not destroy a prior allowance. Pre-approve(100),
/// freeze, submit permit(200) — rollback must leave the prior allowance
/// and its deposit untouched.
///
/// Note: an even stronger test would exercise the cancel-then-approve
/// order directly (cancel succeeds, approve fails, rollback restores).
/// But both pallet-assets entry points gate on `AssetStatus::Live` as
/// their first check, so that exact sequence cannot be constructed in
/// this mock.
#[test]
fn permit_rollback_preserves_prior_allowance() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);
		let deposit: u128 = <Test as pallet_assets::Config>::ApprovalDeposit::get();

		assert_ok!(Assets::approve_transfer(
			RuntimeOrigin::signed(setup.owner_account),
			setup.asset_id,
			setup.spender_account,
			100,
		));
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);

		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, AlloyU256::from(200), setup.deadline);
		assert_ok!(Assets::freeze_asset(
			RuntimeOrigin::signed(setup.owner_account),
			setup.asset_id
		));

		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(200),
			setup.deadline,
			v,
			r,
			s,
		);
		assert_permit_dispatch_err(result, pallet_assets::Error::<Test>::AssetNotLive);

		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100
		);
		assert_eq!(Balances::reserved_balance(&setup.owner_account), deposit);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero()
		);
	});
}

/// `permit(value = uint256.max)` is the gasless infinite-allowance idiom
/// (EIP-2612). `U256::MAX` doesn't fit in the runtime `Balance`, so the
/// precompile saturates the *stored* allowance at `Balance::MAX` rather
/// than reverting at the conversion. Nonce advances normally and the
/// `Approval` event carries the raw signed value.
#[test]
fn permit_saturates_on_uint256_max() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let huge = AlloyU256::MAX;
		let (v, r, s) = sign_permit(setup.asset_addr, setup.spender_addr, huge, setup.deadline);
		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			huge,
			setup.deadline,
			v,
			r,
			s,
		);
		let exec = result.result.expect("permit must not trap");
		assert!(!exec.did_revert(), "permit(uint256.max) must not revert: {:?}", exec);

		// Stored allowance is saturated to `Balance::MAX`; nonce advanced.
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			u128::MAX,
		);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one(),
		);

		// Event carries the raw signed value, not the saturated stored amount.
		assert_contract_event(
			setup.asset_addr,
			IERC20Events::Approval(IERC20::Approval {
				owner: HARDHAT_ACCOUNT_0.0.into(),
				spender: setup.spender_addr.0.into(),
				value: huge,
			}),
		);
	});
}

/// Mirrors `approve_saturates_above_balance_max`: pins the invariant that
/// saturation applies to *any* `U256` exceeding `Balance::MAX`, not only the
/// `U256::MAX` sentinel. Both `approve` and `permit` go through the same
/// `unique_saturated_into()` conversion, so a regression that scopes the
/// saturation to the sentinel would break this path identically.
#[test]
fn permit_saturates_just_above_balance_max() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		// Smallest `U256` that doesn't fit in the mock's `Balance` (u128).
		let just_over = AlloyU256::from(u128::MAX) + AlloyU256::from(1u64);
		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, just_over, setup.deadline);
		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			just_over,
			setup.deadline,
			v,
			r,
			s,
		);
		let exec = result.result.expect("permit must not trap");
		assert!(!exec.did_revert(), "permit(u128::MAX + 1) must not revert: {:?}", exec);

		// Stored allowance is saturated to `Balance::MAX`; nonce advanced.
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			u128::MAX,
		);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one(),
		);

		// Event carries the raw signed value, not the saturated stored amount.
		assert_contract_event(
			setup.asset_addr,
			IERC20Events::Approval(IERC20::Approval {
				owner: HARDHAT_ACCOUNT_0.0.into(),
				spender: setup.spender_addr.0.into(),
				value: just_over,
			}),
		);
	});
}

/// If the owner can't afford the `ApprovalDeposit`, `do_approve_transfer`
/// returns a `DispatchError` (Error::Error → trap). Distinct failure
/// path from the revert-based `to_balance` test.
#[test]
fn permit_rejects_when_owner_lacks_deposit_balance() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		Balances::make_free_balance_be(&setup.owner_account, 0);

		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, AlloyU256::from(100), setup.deadline);
		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			v,
			r,
			s,
		);
		assert_permit_dispatch_err(result, pallet_balances::Error::<Test>::InsufficientBalance);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero(),
			"nonce must not advance when the deposit reserve fails"
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			0
		);
		assert_no_contract_event_from(setup.asset_addr);
	});
}

/// A signature for asset A must NOT be replayable against asset B —
/// pins the `verifyingContract` field of the EIP-712 domain. We register
/// the same underlying asset under both prefixes, sign for one, submit
/// to the other; both directions are tested.
#[test_case(PRECOMPILE_ADDRESS_PREFIX, PRECOMPILE_ADDRESS_PREFIX_FOREIGN)]
#[test_case(PRECOMPILE_ADDRESS_PREFIX_FOREIGN, PRECOMPILE_ADDRESS_PREFIX)]
fn permit_signature_bound_to_verifying_contract(sign_prefix: u16, submit_prefix: u16) {
	new_test_ext().execute_with(|| {
		let setup = permit_setup(sign_prefix);
		if sign_prefix != PRECOMPILE_ADDRESS_PREFIX_FOREIGN &&
			submit_prefix == PRECOMPILE_ADDRESS_PREFIX_FOREIGN
		{
			crate::pallet::Pallet::<Test>::insert_asset_mapping(&setup.asset_id)
				.expect("foreign asset mapping must insert");
		}

		let asset_addr_signed = setup.asset_addr;
		let asset_addr_submitted = H160::from(set_prefix_in_address(submit_prefix));
		assert_ne!(asset_addr_signed, asset_addr_submitted);

		let (v, r, s) = sign_permit(
			asset_addr_signed,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
		);

		let result = raw_permit(
			setup.submitter,
			asset_addr_submitted,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			v,
			r,
			s,
		);
		assert_permit_reverted_with(result, "Signer does not match owner");
		assert_eq!(
			permit::Pallet::<Test>::nonce(&asset_addr_signed, &HARDHAT_ACCOUNT_0),
			U256::zero()
		);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&asset_addr_submitted, &HARDHAT_ACCOUNT_0),
			U256::zero()
		);
	});
}

/// Renaming an asset invalidates outstanding permits — the EIP-712
/// domain separator binds the asset's current `name` metadata. Kept
/// parametrized over both prefixes for confidence on this
/// security-relevant invariant.
#[test_case(PRECOMPILE_ADDRESS_PREFIX)]
#[test_case(PRECOMPILE_ADDRESS_PREFIX_FOREIGN)]
fn permit_rejects_after_token_name_change(asset_index: u16) {
	new_test_ext().execute_with(|| {
		let setup = permit_setup(asset_index);

		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, AlloyU256::from(100), setup.deadline);

		assert_ok!(Assets::force_set_metadata(
			RuntimeOrigin::root(),
			setup.asset_id,
			b"Renamed Token".to_vec(),
			b"RNM".to_vec(),
			18,
			false,
		));

		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			v,
			r,
			s,
		);
		assert_permit_reverted_with(result, "Signer does not match owner");
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero()
		);
	});
}

/// EIP-2612 forbids the zero address as `owner`. The early
/// `owner.is_zero()` check inside `do_verify_permit` runs before signature
/// verification, so dummy `(v, r, s)` is fine.
#[test]
fn permit_rejects_zero_owner() {
	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			H160::zero(),
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			27,
			[0u8; 32],
			[0u8; 32],
		);
		assert_permit_reverted_with(result, "Invalid owner address");
		assert_eq!(Balances::reserved_balance(&setup.owner_account), 0);
		// Nonce on the (zero) owner the call would have advanced, plus
		// nonce on the real signer for good measure — both must stay 0
		// to pin the early-reject ordering.
		assert_eq!(permit::Pallet::<Test>::nonce(&setup.asset_addr, &H160::zero()), U256::zero(),);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero(),
		);
		assert_no_contract_event_from(setup.asset_addr);
	});
}

/// EIP-2612 forbids the zero address as `spender`. Same rationale as
/// `permit_rejects_zero_owner` — the spender zero-check runs before
/// signature verification.
#[test]
fn permit_rejects_zero_spender() {
	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			H160::zero(),
			AlloyU256::from(100),
			setup.deadline,
			27,
			[0u8; 32],
			[0u8; 32],
		);
		assert_permit_reverted_with(result, "Invalid spender address");
		assert_eq!(Balances::reserved_balance(&setup.owner_account), 0);
		// Nonce on the declared owner must stay 0; the early-reject
		// ordering would be broken if it advanced.
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero(),
		);
		assert_no_contract_event_from(setup.asset_addr);
	});
}

/// The deadline check uses strict `deadline < now` — a permit with
/// `deadline == now` must be accepted. Pins this boundary against an
/// inadvertent flip to `<=`. Not covered at the pallet level.
#[test]
fn permit_accepts_deadline_at_current_timestamp() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let now_seconds: u64 = 2_000_000_000;
		pallet_timestamp::Pallet::<Test>::set_timestamp(now_seconds * 1_000);

		let deadline = AlloyU256::from(now_seconds);
		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(100),
			deadline,
		);

		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			100
		);
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::one()
		);
	});
}

/// Exercises the `secp256k1_ecdsa_recover` failure path — one of the
/// permit-pallet error variants the dispatcher maps to a revert string.
/// `r = 0` is not a valid signature component (the implied curve point
/// would have x = 0, but `0³ + 7` has no square root mod p on
/// secp256k1), so recovery returns `Err`.
///
/// Caveat: `"Invalid signature"` is a prefix of `"Invalid signature v
/// value"`, so the substring matcher cannot, on its own, distinguish
/// the two reasons. The test inputs are constructed so the v-range
/// branch is unreachable (`v = 27`, `s = 0` in lower half), making
/// recovery failure the only "Invalid signature*" error this path can
/// fire.
#[test]
fn permit_rejects_recovery_failure() {
	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let result = raw_permit(
			setup.submitter,
			setup.asset_addr,
			HARDHAT_ACCOUNT_0,
			setup.spender_addr,
			AlloyU256::from(100),
			setup.deadline,
			27,
			[0u8; 32],
			[0u8; 32],
		);
		assert_permit_reverted_with(result, "Invalid signature");
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero()
		);
		assert_no_contract_event_from(setup.asset_addr);
	});
}

/// `permit()` is a state-changing call and must be rejected inside a
/// STATICCALL context. The dispatcher's read-only check (the match arm
/// guarding `transfer | approve | transferFrom | permit` against
/// `env.is_read_only()`) is what guards this.
///
/// The test passes a *valid* signature so the post-call state acts as
/// the regression pin: with the dispatcher check active, the call is
/// rejected via `StateChangeDenied`, the writes never run, and
/// nonce/allowance stay at 0. With the check removed, the precompile
/// would proceed past `use_permit` and `do_approve_transfer` (both go
/// through frame_support storage writes that bypass pallet-revive's
/// host-call read-only gating), and nonce would advance to 1. So a
/// regression that drops `IERC20Calls::permit(_)` from the read-only
/// match arm flips this test, even though the outer `success=false`
/// boolean alone would not (an empty trap and a clean revert both
/// surface as `success=false`).
#[test]
fn permit_staticcall_is_rejected() {
	use frame_support::traits::fungibles::approvals::Inspect;

	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let deployer = DEPLOYER_ACCOUNT;
		Balances::make_free_balance_be(&deployer, SUBMITTER_FUNDING);

		let (init_code, _) = pallet_revive_fixtures::compile_module_with_type(
			"Caller",
			pallet_revive_fixtures::FixtureType::Solc,
		)
		.expect("Caller fixture must be compiled");
		let caller_addr = pallet_revive::Pallet::<Test>::bare_instantiate(
			RuntimeOrigin::signed(deployer),
			0u32.into(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			Code::Upload(init_code),
			vec![],
			None,
			&ExecConfig::new_substrate_tx(),
		)
		.result
		.expect("Caller deployment must succeed")
		.addr;

		// Valid permit at the current nonce — same digest the
		// precompile would recompute.
		let (v, r, s) =
			sign_permit(setup.asset_addr, setup.spender_addr, AlloyU256::from(100), setup.deadline);
		let permit_calldata = IERC20::permitCall {
			owner: HARDHAT_ACCOUNT_0.0.into(),
			spender: setup.spender_addr.0.into(),
			value: AlloyU256::from(100),
			deadline: setup.deadline,
			v,
			r: r.into(),
			s: s.into(),
		}
		.abi_encode();

		let calldata = ICaller::staticCallCall {
			callee: alloy::primitives::Address::from(setup.asset_addr.0),
			data: permit_calldata.into(),
			gas: u64::MAX,
		}
		.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(deployer),
			caller_addr,
			0u32.into(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			calldata,
			&ExecConfig::new_substrate_tx(),
		)
		.result
		.expect("outer call must succeed");

		let ret = ICaller::staticCallCall::abi_decode_returns(&result.data)
			.expect("return must decode as (bool, bytes)");
		assert!(!ret.success, "STATICCALL to permit() must be rejected");
		// Regression pin: if the dispatcher's read-only check were
		// dropped, these would both move (nonce → 1, allowance → 100).
		assert_eq!(
			permit::Pallet::<Test>::nonce(&setup.asset_addr, &HARDHAT_ACCOUNT_0),
			U256::zero(),
			"nonce must not advance under STATICCALL",
		);
		assert_eq!(
			Assets::allowance(setup.asset_id, &setup.owner_account, &setup.spender_account),
			0,
			"allowance must not be set under STATICCALL",
		);
	});
}

/// Drives `nonces(owner)` through `bare_call` to pin the dispatch arm
/// for the `nonces` selector. A regression that mis-routes the selector
/// would not be caught by tests that read `permit::Pallet::nonce`
/// storage directly.
#[test]
fn nonces_via_precompile() {
	new_test_ext().execute_with(|| {
		let setup = permit_setup(PRECOMPILE_ADDRESS_PREFIX);

		let read_nonce = |asset_addr: H160| -> AlloyU256 {
			let data = IERC20::noncesCall { owner: HARDHAT_ACCOUNT_0.0.into() }.abi_encode();
			let bytes = pallet_revive::Pallet::<Test>::bare_call(
				RuntimeOrigin::signed(setup.submitter),
				asset_addr,
				0u32.into(),
				TransactionLimits::WeightAndDeposit {
					weight_limit: Weight::MAX,
					deposit_limit: u128::MAX,
				},
				data,
				&ExecConfig::new_substrate_tx(),
			)
			.result
			.expect("nonces() call must succeed")
			.data;
			IERC20::noncesCall::abi_decode_returns(&bytes).expect("decode nonces return")
		};

		assert_eq!(read_nonce(setup.asset_addr), AlloyU256::from(0));

		permit_sign_and_call(
			setup.submitter,
			setup.asset_addr,
			setup.spender_addr,
			AlloyU256::from(50),
			setup.deadline,
		);

		assert_eq!(read_nonce(setup.asset_addr), AlloyU256::from(1));
	});
}
