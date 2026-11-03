// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Unit tests related to the `InitiateTransfer` instruction.
//!
//! See [Fellowship RFC 100](https://github.com/polkadot-fellows/rfCs/pull/100),
//! [Fellowship RFC 122](https://github.com/polkadot-fellows/rfCs/pull/122), and the
//! [specification](https://github.com/polkadot-fellows/xcm-format) for more information.

use codec::Encode;
use frame_support::BoundedVec;
use xcm::{latest::AssetTransferFilter, prelude::*};

use super::mock::*;

// The sender and recipient we use across these tests.
const SENDER: [u8; 32] = [0; 32];
const RECIPIENT: [u8; 32] = [1; 32];
const RECIPIENT2: [u8; 32] = [2; 32];

#[test]
fn clears_origin() {
	// Make sure the sender has enough funds to withdraw.
	add_asset(SENDER, (Here, 100u128));

	let xcm_on_dest =
		Xcm(vec![RefundSurplus, DepositAsset { assets: Wild(All), beneficiary: RECIPIENT.into() }]);
	let assets: Assets = (Here, 90u128).into();
	let xcm = Xcm::<TestCall>(vec![
		WithdrawAsset((Here, 100u128).into()),
		PayFees { asset: (Here, 10u128).into() },
		InitiateTransfer {
			destination: Parent.into(),
			remote_fees: Some(AssetTransferFilter::ReserveDeposit(assets.into())),
			preserve_origin: false,
			assets: BoundedVec::new(),
			remote_xcm: xcm_on_dest,
		},
	]);

	let (mut vm, _) = instantiate_executor(SENDER, xcm.clone());

	// Program runs successfully.
	let res = vm.bench_process(xcm);
	assert!(res.is_ok(), "execution error {:?}", res);

	let (dest, sent_message) = sent_xcm().pop().unwrap();
	assert_eq!(dest, Parent.into());
	assert_eq!(sent_message.len(), 6);
	let mut instr = sent_message.inner().iter();
	assert!(matches!(instr.next().unwrap(), ReserveAssetDeposited(..)));
	assert!(matches!(instr.next().unwrap(), PayFees { .. }));
	assert!(matches!(instr.next().unwrap(), ClearOrigin));
	assert!(matches!(instr.next().unwrap(), RefundSurplus));
	assert!(matches!(instr.next().unwrap(), DepositAsset { .. }));
}

#[test]
fn preserves_origin() {
	// Make sure the sender has enough funds to withdraw.
	add_asset(SENDER, (Here, 100u128));

	let xcm_on_dest =
		Xcm(vec![RefundSurplus, DepositAsset { assets: Wild(All), beneficiary: RECIPIENT.into() }]);
	let assets: Assets = (Here, 90u128).into();
	let xcm = Xcm::<TestCall>(vec![
		WithdrawAsset((Here, 100u128).into()),
		PayFees { asset: (Here, 10u128).into() },
		InitiateTransfer {
			destination: Parent.into(),
			remote_fees: Some(AssetTransferFilter::ReserveDeposit(assets.into())),
			preserve_origin: true,
			assets: BoundedVec::new(),
			remote_xcm: xcm_on_dest,
		},
	]);

	let (mut vm, _) = instantiate_executor(SENDER, xcm.clone());

	// Program runs successfully.
	let res = vm.bench_process(xcm);
	assert!(res.is_ok(), "execution error {:?}", res);

	let (dest, sent_message) = sent_xcm().pop().unwrap();
	assert_eq!(dest, Parent.into());
	assert_eq!(sent_message.len(), 6);
	let mut instr = sent_message.inner().iter();
	assert!(matches!(instr.next().unwrap(), ReserveAssetDeposited(..)));
	assert!(matches!(instr.next().unwrap(), PayFees { .. }));
	assert!(matches!(
		instr.next().unwrap(),
		AliasOrigin(origin) if matches!(origin.unpack(), (0, [Parachain(1000), AccountId32 { id: SENDER, network: None }]))
	));
	assert!(matches!(instr.next().unwrap(), RefundSurplus));
	assert!(matches!(instr.next().unwrap(), DepositAsset { .. }));
}

#[test]
fn unpaid_execution_goes_after_origin_alteration() {
	// Make sure the sender has enough funds to withdraw.
	add_asset(SENDER, (Here, 100u128));

	let xcm_on_destination =
		Xcm::builder_unsafe().refund_surplus().deposit_asset(All, RECIPIENT).build();
	let asset: Asset = (Here, 90u128).into();
	let xcm = Xcm::builder()
		.withdraw_asset((Here, 100u128))
		.pay_fees((Here, 10u128))
		.initiate_transfer(
			Parent,
			None, // We specify no remote fees.
			true, // Preserve origin, necessary for `UnpaidExecution`.
			vec![AssetTransferFilter::ReserveDeposit(asset.into())],
			xcm_on_destination,
		)
		.build();

	// We initialize the executor with the SENDER origin, which is not waived.
	let (mut vm, _) = instantiate_executor(SENDER, xcm.clone());

	// Program fails with `BadOrigin`.
	let result = vm.bench_process(xcm);
	assert!(result.is_ok(), "execution error {:?}", result);

	let (destination, sent_message) = sent_xcm().pop().unwrap();
	assert_eq!(destination, Parent.into());
	assert_eq!(sent_message.len(), 6);
	let mut instructions = sent_message.inner().iter();
	assert!(matches!(instructions.next().unwrap(), ReserveAssetDeposited(..)));
	assert!(matches!(
		instructions.next().unwrap(),
		AliasOrigin(origin) if matches!(origin.unpack(), (0, [Parachain(1000), AccountId32 { id: SENDER, network: None }]))
	));
	assert!(matches!(instructions.next().unwrap(), UnpaidExecution { .. }));
	assert!(matches!(instructions.next().unwrap(), RefundSurplus));
	assert!(matches!(instructions.next().unwrap(), DepositAsset { .. }));
}

#[test]
fn no_alias_origin_if_root() {
	// Make sure the sender has enough funds to withdraw.
	add_asset(Here, (Here, 100u128));

	let xcm_on_destination =
		Xcm::builder_unsafe().refund_surplus().deposit_asset(All, RECIPIENT).build();
	let asset: Asset = (Here, 90u128).into();
	let xcm = Xcm::builder()
		.withdraw_asset((Here, 100u128))
		.pay_fees((Here, 10u128))
		.initiate_transfer(
			Parent,
			None, // We specify no remote fees.
			true, // Preserve origin, necessary for `UnpaidExecution`.
			vec![AssetTransferFilter::ReserveDeposit(asset.into())],
			xcm_on_destination,
		)
		.build();

	// We initialize the executor with the SENDER origin, which is not waived.
	let (mut vm, _) = instantiate_executor(Here, xcm.clone());

	// Program fails with `BadOrigin`.
	let result = vm.bench_process(xcm);
	assert!(result.is_ok(), "execution error {:?}", result);

	let (destination, sent_message) = sent_xcm().pop().unwrap();
	assert_eq!(destination, Parent.into());
	assert_eq!(sent_message.len(), 5);
	let mut instructions = sent_message.inner().iter();
	assert!(matches!(instructions.next().unwrap(), ReserveAssetDeposited(..)));
	assert!(matches!(instructions.next().unwrap(), UnpaidExecution { .. }));
	assert!(matches!(instructions.next().unwrap(), RefundSurplus));
	assert!(matches!(instructions.next().unwrap(), DepositAsset { .. }));
}

// We simulate going from one system parachain to another without
// having to pay remote fees.
#[test]
fn unpaid_transact() {
	let to_another_system_para: Location = (Parent, Parachain(1001)).into();
	// We want to execute some call in the receiving chain.
	let xcm_on_destination = Xcm::builder_unsafe()
		.transact(OriginKind::Superuser, None, b"".encode())
		.build();
	let xcm = Xcm::builder_unsafe()
		.initiate_transfer(
			to_another_system_para.clone(),
			None,   // We specify no remote fees.
			true,   // Preserve necessary for `UnpaidExecution`.
			vec![], // No need for assets.
			xcm_on_destination,
		)
		.build();

	// We initialize the executor with the root origin, which is waived.
	let (mut vm, _) = instantiate_executor(Here, xcm.clone());

	// Program executes successfully.
	let result = vm.bench_process(xcm.clone());
	assert!(result.is_ok(), "execution error: {:?}", result);

	let (destination, sent_message) = sent_xcm().pop().unwrap();
	assert_eq!(destination, to_another_system_para);
	assert_eq!(sent_message.len(), 3);
	let mut instructions = sent_message.inner().iter();
	assert!(matches!(instructions.next().unwrap(), UnpaidExecution { .. }));
	assert!(matches!(instructions.next().unwrap(), Transact { .. }));
}

#[test]
fn deposit_assets_with_retry_aborts_on_failure_and_post_process_traps() {
	// fund sender
	add_asset(SENDER, (Here, 200u128));

	// sub-ED amount (< ED=2): this `DepositAsset` will fail and abort the program.
	let dust: Asset = (Here, 1u128).into();

	// ≥ED amount: would succeed on its own — but the program aborts at the first error,
	// so this instruction never runs.
	let legit: Asset = (Here, 100u128).into();

	let xcm = Xcm::<TestCall>(vec![
		WithdrawAsset((Here, 101u128).into()),
		DepositAsset { assets: Definite(Assets::from(vec![dust])), beneficiary: RECIPIENT.into() },
		DepositAsset { assets: Definite(Assets::from(vec![legit])), beneficiary: RECIPIENT.into() },
	]);

	let (mut vm, weight) = instantiate_executor(SENDER, xcm.clone());

	let err = vm
		.bench_process(xcm)
		.expect_err("a sub-ED `DepositAsset` must error out and abort the rest of the XCM");
	vm.set_error(Some((err.index, err.xcm_error)));

	let outcome = vm.bench_post_process(weight);
	assert!(
		matches!(outcome, Outcome::Incomplete { .. }),
		"Expected Incomplete, got {:?}",
		outcome
	);

	// Nothing reached the recipient: the failing `DepositAsset` rolled back its own take,
	// and the subsequent `DepositAsset(legit)` was never executed because the program is
	// already in error state.
	let here_assets = asset_list(RECIPIENT);
	assert!(here_assets.is_empty(), "no deposit should reach recipient when program aborts");

	// `post_process` trapped the leftover holding — the full 101 originally withdrawn.
	let trapped = asset_list(TRAPPED_ASSETS);
	assert_eq!(
		trapped,
		vec![(Here, 101u128).into()],
		"the entire pre-instruction holding must be trapped (claimable later), not silently lost"
	);
}

#[test]
fn deposit_assets_with_retry_all_failures_aborts_and_traps() {
	// fund sender
	add_asset(SENDER, (Here, 20u128));

	// two sub-ED amounts, both < ED=2
	let d1: Asset = (Here, 1u128).into();
	let d2: Asset = (Here, 1u128).into();

	let xcm = Xcm::<TestCall>(vec![
		// withdraw 1+1 so the withdraw itself succeeds.
		WithdrawAsset((Here, (1u128 + 1u128)).into()),
		DepositAsset { assets: Definite(Assets::from(vec![d1])), beneficiary: RECIPIENT.into() },
		DepositAsset { assets: Definite(Assets::from(vec![d2])), beneficiary: RECIPIENT2.into() },
	]);

	let (mut vm, weight) = instantiate_executor(SENDER, xcm.clone());
	let err = vm.bench_process(xcm).expect_err("first sub-ED `DepositAsset` must abort");
	vm.set_error(Some((err.index, err.xcm_error)));

	let outcome = vm.bench_post_process(weight);
	assert!(matches!(outcome, Outcome::Incomplete { .. }));

	// Neither recipient sees anything.
	assert!(asset_list(RECIPIENT).is_empty(), "sub-ED deposit can't reach a fresh recipient");
	assert!(asset_list(RECIPIENT2).is_empty(), "second instruction never runs after the abort");

	// `post_process` trapped the entire restored holding (2 from the original
	// `WithdrawAsset`).
	let trapped = asset_list(TRAPPED_ASSETS);
	assert_eq!(trapped, vec![(Here, 2u128).into()], "undeposited assets must be trapped, not lost");
}
