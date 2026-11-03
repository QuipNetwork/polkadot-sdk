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

use crate::{ChargePGAS, Event, Val, mock::*};

use frame_support::{assert_ok, weights::Weight};
use pallet_balances::Call as BalancesCall;
use pallet_transaction_payment::ChargeTransactionPayment;
use sp_runtime::traits::{DispatchTransaction, TransactionExtension, TxBaseImplication};

type Ext = ChargePGAS<Runtime, ChargeTransactionPayment<Runtime>>;

fn new_ext() -> Ext {
	ChargePGAS::from(ChargeTransactionPayment::<Runtime>::from(0))
}

fn pgas_call() -> RuntimeCall {
	RuntimeCall::System(frame_system::Call::remark { remark: Default::default() })
}

fn non_pgas_call() -> RuntimeCall {
	RuntimeCall::Balances(BalancesCall::transfer_allow_death { dest: BOB, value: 1 })
}

/// Alice holds no native but enough PGAS. A filter-matching call is paid by burning PGAS;
/// her native balance is untouched.
#[test]
fn pgas_pays_for_filtered_call_with_zero_native() {
	let pgas_initial = 1_000;
	ExtBuilder::default()
		.with_pgas(vec![(ALICE, pgas_initial)])
		.build()
		.execute_with(|| {
			let call = pgas_call();
			let len = 10;
			let info = info_from_weight(Weight::from_parts(7, 0));

			let fee =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, 0);
			assert!(fee > 0);

			assert_eq!(Balances::free_balance(ALICE), 0);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial);

			let (pre, _) = new_ext()
				.validate_and_prepare(Some(ALICE).into(), &call, &info, len, 0)
				.unwrap();

			assert_eq!(Balances::free_balance(ALICE), 0);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial - fee);

			assert_ok!(<Ext as sp_runtime::traits::TransactionExtension<RuntimeCall>>::post_dispatch_details(
				pre,
				&info,
				&default_post_info(),
				len,
				&Ok(()),
			));
			assert_eq!(Balances::free_balance(ALICE), 0);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial - fee);

			System::assert_has_event(Event::PGASFeePaid { who: ALICE, actual_fee: fee }.into());
		});
}

/// Bob holds native but no PGAS. A filter-matching call falls through to the inner extension
/// and is paid in native.
#[test]
fn falls_back_to_inner_when_no_pgas() {
	let native_initial = 1_000;
	ExtBuilder::default()
		.with_native(vec![(BOB, native_initial)])
		.build()
		.execute_with(|| {
			let call = pgas_call();
			let len = 10;
			let info = info_from_weight(Weight::from_parts(7, 0));

			let fee =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, 0);
			assert!(fee > 0);

			assert_eq!(Balances::free_balance(BOB), native_initial);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, BOB), 0);

			let (_pre, _) =
				new_ext().validate_and_prepare(Some(BOB).into(), &call, &info, len, 0).unwrap();

			assert_eq!(Balances::free_balance(BOB), native_initial - fee);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, BOB), 0);
		});
}

/// Charlie holds both native and PGAS but dispatches a call the filter rejects. The inner
/// extension must charge the native fee; PGAS stays untouched.
#[test]
fn filter_miss_uses_inner_even_with_pgas() {
	let native_initial = 1_000;
	let pgas_initial = 1_000;
	ExtBuilder::default()
		.with_native(vec![(CHARLIE, native_initial), (BOB, 10)])
		.with_pgas(vec![(CHARLIE, pgas_initial)])
		.build()
		.execute_with(|| {
			let call = non_pgas_call();
			let len = 10;
			let info = info_from_weight(Weight::from_parts(7, 0));

			let fee =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, 0);
			assert!(fee > 0);

			let (_pre, _) = new_ext()
				.validate_and_prepare(Some(CHARLIE).into(), &call, &info, len, 0)
				.unwrap();

			assert_eq!(Balances::free_balance(CHARLIE), native_initial - fee);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, CHARLIE), pgas_initial);
		});
}

/// Unused weight must be refunded by minting PGAS back to the payer.
#[test]
fn pgas_refund_on_unused_weight() {
	let pgas_initial = 1_000;
	ExtBuilder::default()
		.with_pgas(vec![(ALICE, pgas_initial)])
		.build()
		.execute_with(|| {
			let call = pgas_call();
			let len = 10;
			let claimed = Weight::from_parts(100, 0);
			let actual = Weight::from_parts(40, 0);
			let info = info_from_weight(claimed);

			let reserved =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, 0);
			let actual_fee = pallet_transaction_payment::Pallet::<Runtime>::compute_actual_fee(
				len as u32,
				&info,
				&post_info_from_weight(actual),
				0,
			);
			assert!(reserved > actual_fee);

			let (pre, _) = new_ext()
				.validate_and_prepare(Some(ALICE).into(), &call, &info, len, 0)
				.unwrap();
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial - reserved);

			assert_ok!(<Ext as sp_runtime::traits::TransactionExtension<RuntimeCall>>::post_dispatch_details(
				pre,
				&info,
				&post_info_from_weight(actual),
				len,
				&Ok(()),
			));
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial - actual_fee);
		});
}

/// PGAS is `Expendable`: paying a fee that drops the balance below ED dusts the account
/// rather than falling back to native. PGAS is meant to be minted across many accounts per
/// user, so reaping a dusted PGAS account is acceptable.
#[test]
fn pgas_below_ed_dusts_account() {
	let native_initial = 1_000;
	// Asset ED is 1 (see `ExtBuilder::build`). Give Alice exactly the fee in PGAS so paying it
	// drains the balance to zero.
	ExtBuilder::default()
		.with_native(vec![(ALICE, native_initial)])
		.build()
		.execute_with(|| {
			let call = pgas_call();
			let len = 10;
			let info = info_from_weight(Weight::from_parts(7, 0));

			let fee = pallet_transaction_payment::Pallet::<Runtime>::compute_fee(
				len as u32,
				&info,
				0,
			);
			assert!(fee > 0);

			let pgas_initial = fee;
			assert_ok!(<pallet_assets::Pallet<Runtime> as frame_support::traits::tokens::fungibles::Mutate<AccountId>>::mint_into(
				PGAS_ASSET_ID,
				&ALICE,
				pgas_initial,
			));
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial);

			let (_pre, _) = new_ext()
				.validate_and_prepare(Some(ALICE).into(), &call, &info, len, 0)
				.unwrap();

			// PGAS drained to 0 (account dusted), native untouched.
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), 0);
			assert_eq!(Balances::free_balance(ALICE), native_initial);
		});
}

/// Unsigned origins skip the PGAS path entirely and go straight to the inner extension.
#[test]
fn unsigned_delegates_to_inner() {
	ExtBuilder::default().with_pgas(vec![(ALICE, 1_000)]).build().execute_with(|| {
		let call = pgas_call();
		let len = 10;
		let info = info_from_weight(Weight::from_parts(7, 0));

		let (_, val, _) = <Ext as TransactionExtension<RuntimeCall>>::validate(
			&new_ext(),
			frame_system::RawOrigin::None.into(),
			&call,
			&info,
			len,
			(),
			&TxBaseImplication((0u8, &call)),
			sp_runtime::transaction_validity::TransactionSource::External,
		)
		.unwrap();
		assert!(matches!(val, Val::Inner(_)));
	});
}

/// An extension built with `new_skip_pgas` always delegates to the inner extension, even when
/// the caller holds enough PGAS and the call passes the filter. The native balance pays the
/// fee and PGAS is untouched.
#[test]
fn skip_pgas_always_delegates_to_inner() {
	let native_initial = 1_000;
	let pgas_initial = 1_000;
	ExtBuilder::default()
		.with_native(vec![(ALICE, native_initial)])
		.with_pgas(vec![(ALICE, pgas_initial)])
		.build()
		.execute_with(|| {
			let call = pgas_call();
			let len = 10;
			let info = info_from_weight(Weight::from_parts(7, 0));

			let fee =
				pallet_transaction_payment::Pallet::<Runtime>::compute_fee(len as u32, &info, 0);
			assert!(fee > 0);

			let ext = ChargePGAS::new_skip_pgas(ChargeTransactionPayment::<Runtime>::from(0));
			let (_, val, _) = <Ext as TransactionExtension<RuntimeCall>>::validate(
				&ext,
				Some(ALICE).into(),
				&call,
				&info,
				len,
				(),
				&TxBaseImplication((0u8, &call)),
				sp_runtime::transaction_validity::TransactionSource::External,
			)
			.unwrap();
			assert!(matches!(val, Val::Inner(_)));

			let (_pre, _) =
				ext.validate_and_prepare(Some(ALICE).into(), &call, &info, len, 0).unwrap();

			assert_eq!(Balances::free_balance(ALICE), native_initial - fee);
			assert_eq!(Assets::balance(PGAS_ASSET_ID, ALICE), pgas_initial);
		});
}
