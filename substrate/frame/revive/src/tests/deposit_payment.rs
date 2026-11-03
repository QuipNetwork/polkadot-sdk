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

//! Tests for the [`PGasDeposit`] storage-deposit backend.

use crate::{
	Code, Config, DeletionQueue, FreezeReason, HoldReason, NativeDepositOf,
	deposit_payment::{Deposit, Funds},
	test_utils::{
		ALICE, BOB, CHARLIE, DJANGO_ADDR,
		builder::{BareCallBuilder, Contract},
	},
	tests::{
		Assets, AssetsFreezer, AssetsHolder, Balances, Contracts, ExtBuilder, PGAS_ASSET_ID,
		RuntimeOrigin, System, Test, builder, test_utils::get_contract_checked,
	},
};
use alloy_core::sol_types::SolCall;
use frame_support::{
	assert_ok,
	traits::{
		OnIdle,
		fungible::{Inspect as _, InspectHold, Mutate as _},
		tokens::{
			Fortitude, Precision, Preservation,
			fungibles::{
				Inspect as FungiblesInspect, InspectFreeze as FungiblesInspectFreeze,
				InspectHold as _, Mutate as FungiblesMutate,
			},
		},
	},
	weights::Weight,
};
use pallet_revive_fixtures::{
	FixtureType, MultiContributorStorage, compile_module, compile_module_with_type,
};
use pretty_assertions::assert_eq;
use sp_runtime::{AccountId32, DispatchResult};
use test_case::test_case;

/// Full observable state snapshot for a (payer, contract) pair.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct State {
	/// Payer's free native currency balance.
	payer_native: u128,
	/// Payer's free PGAS balance.
	payer_pgas: u128,
	/// Native currency currently held on the contract.
	contract_native_held: u128,
	/// PGAS currently held on the contract.
	contract_pgas_held: u128,
	/// `NativeDepositOf[contract][payer]`: the payer's outstanding native-currency
	native_entitlement: u128,
}

fn snapshot(payer: &AccountId32, contract: &AccountId32) -> State {
	let hold = HoldReason::StorageDepositReserve.into();
	State {
		payer_native: Balances::free_balance(payer),
		payer_pgas: Assets::balance(PGAS_ASSET_ID, payer),
		contract_native_held: Balances::balance_on_hold(&hold, contract),
		contract_pgas_held: AssetsHolder::balance_on_hold(PGAS_ASSET_ID, &hold, contract),
		native_entitlement: NativeDepositOf::<Test>::get(contract, payer),
	}
}

/// One charge with the state expected immediately afterwards.
struct Charge {
	/// Payer account.
	payer: AccountId32,
	/// Amount to charge.
	amount: u128,
	/// Expected state for the payer and contract right after the charge lands.
	expected: State,
}

/// Initial balances for one account.
struct AccountSetup {
	account: AccountId32,
	native: u128,
	pgas: u128,
}

/// A full scenario: initial balances, a sequence of charges, then one refund.
struct TestCase {
	/// Starting balances.
	accounts: Vec<AccountSetup>,
	/// Sequential charges applied to the contract account.
	charges: Vec<Charge>,
	/// Refund recipient and amount.
	refund: (AccountId32, u128),
	/// Expected states keyed by account after the refund.
	expected_after_refund: Vec<(AccountId32, State)>,
}

fn charge_and_hold(from: &AccountId32, to: &AccountId32, amount: u128) -> DispatchResult {
	<<Test as Config>::Deposit as Deposit<Test>>::charge_and_hold(
		HoldReason::StorageDepositReserve,
		Funds::Balance(from),
		to,
		amount,
	)
}

fn refund_on_hold(from: &AccountId32, to: &AccountId32, amount: u128) -> DispatchResult {
	<<Test as Config>::Deposit as Deposit<Test>>::refund_on_hold(
		HoldReason::StorageDepositReserve,
		from,
		Funds::Balance(to),
		amount,
	)
}

fn run(TestCase { accounts, charges, refund, expected_after_refund }: TestCase) {
	let pgas_balances = accounts
		.iter()
		.filter(|account| account.pgas > 0)
		.map(|account| (account.account.clone(), account.pgas))
		.collect();

	ExtBuilder::default()
		.with_pgas_balances(pgas_balances)
		.build()
		.execute_with(|| {
			for AccountSetup { account, native, .. } in accounts {
				Balances::set_balance(&account, native);
			}
			// Mint the native and PGAS ED onto the contract account, mirroring what
			// `init_contract` does at contract creation time.
			assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

			for (i, charge) in charges.iter().enumerate() {
				assert_ok!(charge_and_hold(&charge.payer, &BOB, charge.amount));
				assert_eq!(snapshot(&charge.payer, &BOB), charge.expected, "after charge {i}");
			}

			assert_ok!(refund_on_hold(&BOB, &refund.0, refund.1));
			for (payer, expected) in expected_after_refund {
				assert_eq!(snapshot(&payer, &BOB), expected, "after refund for {payer:?}");
			}
		});
}

/// Native-only: ALICE has no PGAS, so the 100-unit hold is fully backed by native currency
/// and [`NativeDepositOf`] tracks it; the refund returns the native currency.
#[test]
fn pay_native_refund_native() {
	run(TestCase {
		accounts: vec![AccountSetup { account: ALICE, native: 1_000, pgas: 0 }],
		charges: vec![Charge {
			payer: ALICE,
			amount: 100,
			expected: State {
				payer_native: 900,
				contract_native_held: 100,
				native_entitlement: 100,
				..State::default()
			},
		}],
		refund: (ALICE, 100),
		expected_after_refund: vec![(ALICE, State { payer_native: 1_000, ..State::default() })],
	});
}

/// PGAS-only: ALICE's PGAS covers the hold, so native currency is untouched and no
/// entitlement is recorded; the refund returns PGAS (at `PGasRefundPercent`).
#[test]
fn pay_pgas_refund_pgas() {
	run(TestCase {
		accounts: vec![AccountSetup { account: ALICE, native: 1_000, pgas: 1_000 }],
		charges: vec![Charge {
			payer: ALICE,
			amount: 100,
			expected: State {
				payer_native: 1_000,
				payer_pgas: 900,
				contract_pgas_held: 100,
				..State::default()
			},
		}],
		refund: (ALICE, 100),
		expected_after_refund: vec![(
			ALICE,
			State { payer_native: 1_000, payer_pgas: 910, ..State::default() },
		)],
	});
}

/// Mixed: first charge (40) fits into ALICE's 100 PGAS; second charge (80)
/// exceeds remaining PGAS (60) so falls back to native currency in full. Refund pays native
/// first (capped by the entitlement), then PGAS for the remainder.
#[test]
fn pay_mixed_refund_mixed() {
	run(TestCase {
		accounts: vec![AccountSetup { account: ALICE, native: 1_000, pgas: 100 }],
		charges: vec![
			Charge {
				payer: ALICE,
				amount: 40,
				expected: State {
					payer_native: 1_000,
					payer_pgas: 60,
					contract_pgas_held: 40,
					..State::default()
				},
			},
			Charge {
				payer: ALICE,
				amount: 80,
				expected: State {
					payer_native: 920,
					payer_pgas: 60,
					contract_native_held: 80,
					contract_pgas_held: 40,
					native_entitlement: 80,
				},
			},
		],
		refund: (ALICE, 120),
		expected_after_refund: vec![(
			ALICE,
			State { payer_native: 1_000, payer_pgas: 64, ..State::default() },
		)],
	});
}

/// Sub-ED hold: charge 50 PGAS with PGAS ED = 100, then refund — the 10% refund (5) credits
/// ALICE and the rest (45) is burned. `burn_held` should not reject the sub-ED hold.
#[test]
fn burn_held_on_sub_ed_hold_works() {
	ExtBuilder::default()
		.with_pgas_min_balance(100)
		.with_pgas_balances(vec![(ALICE, 1_000)])
		.build()
		.execute_with(|| {
			Balances::set_balance(&ALICE, 1_000);
			assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

			// PGAS branch: 50 transferred on top of the ED minted by `init_contract`.
			assert_ok!(charge_and_hold(&ALICE, &BOB, 50));
			assert_eq!(
				snapshot(&ALICE, &BOB),
				State {
					payer_native: 1_000,
					payer_pgas: 950,
					contract_pgas_held: 50,
					..State::default()
				},
				"after sub-ED charge",
			);

			// Refund: 10% of 50 = 5 goes to ALICE, 45 is burned via burn_held.
			assert_ok!(refund_on_hold(&BOB, &ALICE, 50));
			assert_eq!(
				snapshot(&ALICE, &BOB),
				State { payer_native: 1_000, payer_pgas: 955, ..State::default() },
				"after refund (5 refunded, 45 burned)",
			);
		});
}

/// Partial sub-ED refund: charge 50 PGAS (below PGAS ED = 100), refund 20. Expect 2 refunded
/// to ALICE, 18 burned, 30 still held.
#[test]
fn burn_held_on_sub_ed_hold_partial_refund() {
	ExtBuilder::default()
		.with_pgas_min_balance(100)
		.with_pgas_balances(vec![(ALICE, 1_000)])
		.build()
		.execute_with(|| {
			Balances::set_balance(&ALICE, 1_000);
			assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

			assert_ok!(charge_and_hold(&ALICE, &BOB, 50));
			assert_ok!(refund_on_hold(&BOB, &ALICE, 20));
			assert_eq!(
				snapshot(&ALICE, &BOB),
				State {
					payer_native: 1_000,
					payer_pgas: 952,
					contract_pgas_held: 30,
					..State::default()
				},
				"after partial refund (2 refunded, 18 burned, 30 still held)",
			);
		});
}

/// `init_contract` mints the native ED (deactivated) and the PGAS ED into the contract.
/// `destroy_contract` is its exact inverse: total_issuance, inactive_issuance, and active
/// issuance all return to their starting values.
#[test]
fn init_and_destroy_contract_round_trip() {
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		let native_total_before = Balances::total_issuance();
		let native_inactive_before = Balances::inactive_issuance();
		let native_active_before = Balances::active_issuance();
		let pgas_total_before = Assets::total_issuance(PGAS_ASSET_ID);

		assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

		// BOB has the native ED in free balance, deactivated.
		assert_eq!(Balances::balance(&BOB), 50, "BOB should have native ED minted");
		assert_eq!(Balances::total_issuance(), native_total_before + 50);
		assert_eq!(Balances::inactive_issuance(), native_inactive_before + 50);
		assert_eq!(
			Balances::active_issuance(),
			native_active_before,
			"deactivate keeps active issuance pinned"
		);

		let pgas_ed = Assets::minimum_balance(PGAS_ASSET_ID);
		assert_eq!(Assets::balance(PGAS_ASSET_ID, &BOB), pgas_ed);
		assert_eq!(Assets::total_issuance(PGAS_ASSET_ID), pgas_total_before + pgas_ed);

		assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::destroy_contract(&BOB));

		assert_eq!(Balances::balance(&BOB), 0, "native ED has been burned out of BOB");
		assert_eq!(Assets::balance(PGAS_ASSET_ID, &BOB), 0);
		assert_eq!(Balances::total_issuance(), native_total_before);
		assert_eq!(Balances::inactive_issuance(), native_inactive_before);
		assert_eq!(Balances::active_issuance(), native_active_before);
		assert_eq!(Assets::total_issuance(PGAS_ASSET_ID), pgas_total_before);
	});
}

/// After `init_contract`, the contract has a PGAS asset account with at least the PGAS ED,
/// so a sub-ED PGAS transfer into it succeeds (would normally fail because transfers below the
/// asset's ED to a fresh account get rejected).
#[test]
fn minted_contract_can_receive_sub_ed_pgas() {
	ExtBuilder::default()
		.with_pgas_min_balance(100)
		.with_pgas_balances(vec![(ALICE, 1_000)])
		.build()
		.execute_with(|| {
			Balances::set_balance(&ALICE, 1_000);
			assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

			assert_eq!(Assets::balance(PGAS_ASSET_ID, &BOB), 100);

			assert_ok!(<Assets as FungiblesMutate<_>>::transfer(
				PGAS_ASSET_ID,
				&ALICE,
				&BOB,
				30,
				Preservation::Preserve,
			));
			assert_eq!(Assets::balance(PGAS_ASSET_ID, &BOB), 130);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, &ALICE), 970);
		});
}

/// After `init_contract`, the contract has a native account too, so a sub-ED native
/// transfer into it also succeeds.
#[test]
fn minted_contract_can_receive_sub_ed_native() {
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		Balances::set_balance(&ALICE, 1_000);
		assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

		assert_eq!(Balances::balance(&BOB), 50);

		assert_ok!(Balances::transfer(&ALICE, &BOB, 10, Preservation::Preserve));
		assert_eq!(Balances::balance(&BOB), 60);
		assert_eq!(Balances::balance(&ALICE), 990);
	});
}

/// The native ED minted by `init_contract` is NOT extractable while the contract has a
/// system consumer. Burning the ED directly is rejected by the underlying balance pallet
/// because `can_dec_provider` is false (consumer pinned).
#[test]
fn minted_contract_native_ed_not_extractable_with_consumer() {
	let (binary, _) = compile_module("dummy").unwrap();
	ExtBuilder::default().existential_deposit(50).build().execute_with(|| {
		Balances::set_balance(&ALICE, 1_000_000);
		let Contract { account_id, .. } =
			builder::bare_instantiate(Code::Upload(binary)).build_and_unwrap_contract();

		let before = Balances::balance(&account_id);
		let result = Balances::burn_from(
			&account_id,
			50,
			Preservation::Expendable,
			Precision::Exact,
			Fortitude::Force,
		);
		assert!(
			result.is_err(),
			"the consumer pin must keep the native ED non-extractable; got {result:?}"
		);
		assert_eq!(Balances::balance(&account_id), before, "balance unchanged");
	});
}

/// The PGAS ED minted by `init_contract` is NOT extractable: it is frozen under
/// [`FreezeReason::PGasMinBalance`]. Pallet-assets' `reducible_balance` returns
/// 0 even with the most permissive flags (`Preservation::Expendable`, `Fortitude::Force`),
/// because `untouchable = max(frozen - held, min_balance) = ED` whenever any freeze exists.
/// A direct transfer of even 1 unit is therefore rejected.
#[test]
fn minted_contract_pgas_ed_not_extractable_due_to_freeze() {
	let pgas_ed = 100u128;
	ExtBuilder::default().with_pgas_min_balance(pgas_ed).build().execute_with(|| {
		assert_ok!(<<Test as Config>::Deposit as Deposit<Test>>::init_contract(&BOB));

		// The ED sits in BOB's free balance and is fully frozen.
		assert_eq!(Assets::balance(PGAS_ASSET_ID, &BOB), pgas_ed);
		assert_eq!(
			<AssetsFreezer as FungiblesInspectFreeze<_>>::balance_frozen(
				PGAS_ASSET_ID,
				&FreezeReason::PGasMinBalance.into(),
				&BOB,
			),
			pgas_ed,
		);

		// Nothing is reducible, even under Expendable + Force.
		assert_eq!(
			<Assets as FungiblesInspect<_>>::reducible_balance(
				PGAS_ASSET_ID,
				&BOB,
				Preservation::Expendable,
				Fortitude::Force,
			),
			0,
			"the freeze pins the ED — Expendable/Force don't override it",
		);

		// A direct transfer of even 1 unit out of BOB is rejected.
		assert!(
			<Assets as FungiblesMutate<_>>::transfer(
				PGAS_ASSET_ID,
				&BOB,
				&ALICE,
				1,
				Preservation::Expendable,
			)
			.is_err(),
			"transfer of 1 unit must fail while the ED is frozen",
		);
	});
}

/// A contract whose storage was paid for by two different signers, both via the native
/// fallback path, can still be terminated. [`Deposit::refund_all`] bypasses the per-payer
/// [`NativeDepositOf`] cap (one recipient at termination, contract gone), so the full native
/// hold goes to the terminator and any PGAS hold is settled via `settle_pgas_refund`.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn refund_all_drains_multi_contributor_native_hold(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("MultiContributorStorage", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		Balances::set_balance(&ALICE, 100_000_000_000);
		Balances::set_balance(&CHARLIE, 100_000_000_000);

		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		assert_ok!(
			builder::bare_call(addr)
				.data(MultiContributorStorage::growStorageCall {}.abi_encode())
				.build()
				.result,
		);
		assert_ok!(
			BareCallBuilder::<Test>::bare_call(RuntimeOrigin::signed(CHARLIE), addr)
				.data(MultiContributorStorage::growStorageCall {}.abi_encode())
				.build()
				.result,
		);

		let alice_entry = NativeDepositOf::<Test>::get(&account_id, &ALICE);
		let charlie_entry = NativeDepositOf::<Test>::get(&account_id, &CHARLIE);
		assert!(alice_entry > 0);
		assert!(charlie_entry > 0);

		let hold: <Test as Config>::RuntimeHoldReason = HoldReason::StorageDepositReserve.into();
		let native_held = Balances::balance_on_hold(&hold, &account_id);
		let pgas_held = AssetsHolder::balance_on_hold(PGAS_ASSET_ID, &hold, &account_id);
		assert_eq!(pgas_held, 0, "every charge fell back to native");
		assert_eq!(native_held, alice_entry + charlie_entry);

		let alice_before = Balances::balance(&ALICE);
		assert_ok!(
			builder::bare_call(addr)
				.data(
					MultiContributorStorage::terminateCall { beneficiary: DJANGO_ADDR.0.into() }
						.abi_encode(),
				)
				.build()
				.result,
		);
		let alice_after = Balances::balance(&ALICE);

		assert!(get_contract_checked(&addr).is_none(), "contract should be gone");
		assert_eq!(
			Balances::balance_on_hold(&hold, &account_id),
			0,
			"the full multi-contributor native hold has been released",
		);
		// ALICE receives the full storage-deposit hold (her own + CHARLIE's). The actual delta
		// also picks up the code-upload deposit refund and any tx-level deposit accounting,
		// so it is at least `native_held`.
		assert!(
			alice_after.saturating_sub(alice_before) >= native_held,
			"expected ALICE balance delta >= {}, got {}",
			native_held,
			alice_after.saturating_sub(alice_before),
		);
	});
}

/// Terminating a contract reaps its system account (native and PGAS EDs are burned by
/// `destroy_contract`, the manual consumer is decremented), and the `on_idle` deletion-queue
/// drain clears its [`NativeDepositOf`] rows. We charge a multi-contributor native deposit
/// first so the double map is genuinely populated and we can observe both rows disappear.
#[test_case(FixtureType::Solc)]
#[test_case(FixtureType::Resolc)]
fn destroy_contract_reaps_account_and_clears_native_deposit_map(fixture_type: FixtureType) {
	let (code, _) = compile_module_with_type("MultiContributorStorage", fixture_type).unwrap();
	ExtBuilder::default().build().execute_with(|| {
		Balances::set_balance(&ALICE, 100_000_000_000);
		Balances::set_balance(&CHARLIE, 100_000_000_000);

		let Contract { addr, account_id } =
			builder::bare_instantiate(Code::Upload(code)).build_and_unwrap_contract();

		// Two distinct payers grow distinct slots so that `NativeDepositOf[contract][_]` has
		// two rows once the deletion queue starts draining.
		assert_ok!(
			builder::bare_call(addr)
				.data(MultiContributorStorage::growStorageCall {}.abi_encode())
				.build()
				.result,
		);
		assert_ok!(
			BareCallBuilder::<Test>::bare_call(RuntimeOrigin::signed(CHARLIE), addr)
				.data(MultiContributorStorage::growStorageCall {}.abi_encode())
				.build()
				.result,
		);

		assert!(NativeDepositOf::<Test>::get(&account_id, &ALICE) > 0);
		assert!(NativeDepositOf::<Test>::get(&account_id, &CHARLIE) > 0);
		assert!(System::account_exists(&account_id), "contract account is alive pre-terminate");

		assert_ok!(
			builder::bare_call(addr)
				.data(
					MultiContributorStorage::terminateCall { beneficiary: DJANGO_ADDR.0.into() }
						.abi_encode(),
				)
				.build()
				.result,
		);

		assert!(get_contract_checked(&addr).is_none(), "contract info should be gone");
		assert!(
			!System::account_exists(&account_id),
			"system account should be reaped once destroy_contract burns the EDs",
		);
		assert_eq!(Balances::balance(&account_id), 0);
		assert_eq!(Assets::balance(PGAS_ASSET_ID, &account_id), 0);

		// `NativeDepositOf` rows survive termination; they're cleared lazily by `on_idle`.
		assert!(NativeDepositOf::<Test>::get(&account_id, &ALICE) > 0);
		assert!(NativeDepositOf::<Test>::get(&account_id, &CHARLIE) > 0);
		assert_eq!(DeletionQueue::<Test>::iter().count(), 1, "contract is queued for deletion");

		Contracts::on_idle(System::block_number(), Weight::MAX);

		assert_eq!(
			DeletionQueue::<Test>::iter().count(),
			0,
			"deletion queue drained to completion",
		);
		assert_eq!(NativeDepositOf::<Test>::iter_prefix(&account_id).count(), 0);
	});
}

/// Refunding a recipient with no [`NativeDepositOf`] credit on a contract whose deposit was
/// paid in native must not revert: the PGAS settlement path is capped at the (empty) PGAS hold
/// and is a no-op, leaving the original payer's native hold intact.
#[test]
fn refund_to_user_without_entitlement_does_not_revert() {
	let after_charge = State {
		payer_native: 900,
		contract_native_held: 100,
		native_entitlement: 100,
		..State::default()
	};
	run(TestCase {
		accounts: vec![AccountSetup { account: ALICE, native: 1_000, pgas: 0 }],
		charges: vec![Charge { payer: ALICE, amount: 100, expected: after_charge }],
		refund: (CHARLIE, 80),
		expected_after_refund: vec![(ALICE, after_charge)],
	});
}

/// Mixed native/PGAS holds must not revert when a PGAS-routed refund request exceeds the
/// contract's PGAS hold. PGAS settlement is capped to the PGAS actually held, and unrelated
/// native entitlements stay with their original contributor.
#[test]
fn mixed_native_pgas_refund_caps_pgas_without_reverting() {
	run(TestCase {
		accounts: vec![
			AccountSetup { account: ALICE, native: 1_000, pgas: 0 },
			AccountSetup { account: CHARLIE, native: 1_000, pgas: 1_000 },
		],
		charges: vec![
			Charge {
				payer: ALICE,
				amount: 100,
				expected: State {
					payer_native: 900,
					contract_native_held: 100,
					native_entitlement: 100,
					..State::default()
				},
			},
			Charge {
				payer: CHARLIE,
				amount: 40,
				expected: State {
					payer_native: 1_000,
					payer_pgas: 960,
					contract_native_held: 100,
					contract_pgas_held: 40,
					..State::default()
				},
			},
		],
		refund: (CHARLIE, 80),
		expected_after_refund: vec![
			(
				ALICE,
				State {
					payer_native: 900,
					contract_native_held: 100,
					native_entitlement: 100,
					..State::default()
				},
			),
			(
				CHARLIE,
				State {
					payer_native: 1_000,
					// CHARLIE pays 40 PGAS, then receives a 10% refund on the capped 40 PGAS
					// settlement: 1_000 - 40 + 4.
					payer_pgas: 964,
					contract_native_held: 100,
					..State::default()
				},
			),
		],
	});
}

/// Code upload and removal with the uploader holding only PGAS.
/// Exercises [`crate::vm::ContractBlob::store_code`] / [`crate::vm::ContractBlob::remove`]
/// against the [`PGasDeposit`] backend. Surfaces any issue from the pallet account not
/// having a pre-existing PGAS asset account.
#[test]
fn code_upload_and_remove_with_pgas() {
	let (binary, code_hash) = compile_module("dummy").unwrap();
	ExtBuilder::default()
		.with_pgas_balances(vec![(ALICE, 10_000_000)])
		.build()
		.execute_with(|| {
			Balances::set_balance(&ALICE, 0);
			let pallet_account = crate::Pallet::<Test>::account_id();

			assert_ok!(Contracts::upload_code(
				RuntimeOrigin::signed(ALICE),
				binary,
				crate::test_utils::deposit_limit::<Test>(),
			));

			let info = crate::CodeInfoOf::<Test>::get(&code_hash).unwrap();
			let deposit = info.deposit();

			assert_eq!(
				AssetsHolder::balance_on_hold(
					PGAS_ASSET_ID,
					&HoldReason::CodeUploadDepositReserve.into(),
					&pallet_account,
				),
				deposit,
				"deposit held in PGAS on the pallet account",
			);
			assert_eq!(
				NativeDepositOf::<Test>::get(&pallet_account, &ALICE),
				0,
				"PGAS path does not record a native entitlement",
			);

			let pgas_before_remove = Assets::balance(PGAS_ASSET_ID, &ALICE);
			assert_ok!(Contracts::remove_code(RuntimeOrigin::signed(ALICE), code_hash));
			let pgas_after_remove = Assets::balance(PGAS_ASSET_ID, &ALICE);

			let refund_pct = crate::tests::PGasRefundPercent::get();
			let expected_refund = refund_pct.mul_floor(deposit);
			assert_eq!(
				pgas_after_remove - pgas_before_remove,
				expected_refund,
				"PGAS partial refund credited to uploader",
			);
			assert_eq!(
				AssetsHolder::balance_on_hold(
					PGAS_ASSET_ID,
					&HoldReason::CodeUploadDepositReserve.into(),
					&pallet_account,
				),
				0,
				"hold released",
			);
		});
}

/// [`Deposit::refund_all`] only releases as much of the storage deposit as the freezes on the
/// account allow. Holds count towards a freeze, so the part of the freeze that the free balance
/// does not cover stays on hold instead of failing the refund.
#[test]
fn refund_all_leaves_frozen_part_on_hold() {
	use frame_support::traits::{LockableCurrency, WithdrawReasons, fungible::MutateHold};

	ExtBuilder::default().build().execute_with(|| {
		let hold: <Test as Config>::RuntimeHoldReason = HoldReason::StorageDepositReserve.into();
		Balances::set_balance(&BOB, 1_000);
		assert_ok!(Balances::hold(&hold, &BOB, 600));
		// 400 free and 600 held: the free balance covers 400 of the lock, the hold the rest.
		Balances::set_lock(*b"test/lck", &BOB, 900, WithdrawReasons::all());

		let alice_before = Balances::balance(&ALICE);
		let refunded =
			<<Test as Config>::Deposit as Deposit<Test>>::refund_all(&BOB, Funds::Balance(&ALICE))
				.unwrap();

		assert_eq!(refunded, 100);
		assert_eq!(Balances::balance_on_hold(&hold, &BOB), 500);
		assert_eq!(Balances::balance(&ALICE), alice_before + 100);
	});
}
