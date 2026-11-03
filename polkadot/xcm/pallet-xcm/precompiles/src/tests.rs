// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.
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

use super::*;
use crate::{
	mock::*,
	IXcm::{self, weighMessageCall},
	VersionedLocation, VersionedXcm,
};
use frame_support::traits::Currency;
use pallet_revive::{
	precompiles::{
		alloy::{
			hex,
			sol_types::{SolCall, SolInterface, SolValue},
		},
		H160,
	},
	Code, ExecConfig, TransactionLimits, U256,
};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_runtime::traits::AccountIdConversion;
use xcm::{prelude::*, v3, v4};

const BOB: AccountId = AccountId::new([1u8; 32]);
const CHARLIE: AccountId = AccountId::new([2u8; 32]);
const SEND_AMOUNT: u128 = 10;
const CUSTOM_INITIAL_BALANCE: u128 = 100_000_000_000u128;

#[test]
fn test_xcm_send_precompile_works() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			ClearOrigin,
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender.clone() },
		]);

		let versioned_dest: VersionedLocation = RelayLocation::get().into();
		let versioned_message: VersionedXcm<()> = VersionedXcm::from(message.clone());

		let xcm_send_params = IXcm::sendCall {
			destination: versioned_dest.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		assert!(result.result.is_ok());
		let sent_message = Xcm(Some(DescendOrigin(sender.clone().try_into().unwrap()))
			.into_iter()
			.chain(message.0.clone().into_iter())
			.collect());
		assert_eq!(sent_xcm(), vec![(Here.into(), sent_message)]);
	});
}

#[test]
fn test_xcm_send_precompile_to_parachain() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			ClearOrigin,
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender.clone() },
		]);

		let destination: VersionedLocation = Parachain(SOME_PARA_ID).into();
		let versioned_message: VersionedXcm<()> = VersionedXcm::from(message.clone());

		let xcm_send_params = IXcm::sendCall {
			destination: destination.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);

		assert!(result.result.is_ok());
		let sent_message = Xcm(Some(DescendOrigin(sender.clone().try_into().unwrap()))
			.into_iter()
			.chain(message.0.clone().into_iter())
			.collect());
		assert_eq!(sent_xcm(), vec![(Parachain(SOME_PARA_ID).into(), sent_message)]);
	});
}

#[test]
fn test_xcm_send_precompile_fails() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender },
		]);

		let destination: VersionedLocation = VersionedLocation::from(Location::ancestor(8));
		let versioned_message: VersionedXcm<()> = VersionedXcm::from(message.clone());

		let xcm_send_params = IXcm::sendCall {
			destination: destination.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmSendPrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());
	});
}

#[test]
fn send_fails_on_old_location_version() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			ClearOrigin,
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender.clone() },
		]);

		// V4 location is old and will fail.
		let destination: VersionedLocation =
			VersionedLocation::V4(v4::Junction::Parachain(SOME_PARA_ID).into());
		let versioned_message: VersionedXcm<RuntimeCall> = VersionedXcm::from(message.clone());

		let xcm_send_params = IXcm::sendCall {
			destination: destination.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmSendPrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());

		// V3 also fails.
		let destination: VersionedLocation =
			VersionedLocation::V3(v3::Junction::Parachain(SOME_PARA_ID).into());
		let versioned_message: VersionedXcm<RuntimeCall> = VersionedXcm::from(message);

		let xcm_send_params = IXcm::sendCall {
			destination: destination.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmSendPrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());
	});
}

#[test]
fn send_fails_on_old_xcm_version() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			ClearOrigin,
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender.clone() },
		]);
		// V4 is old and fails.
		let v4_message: v4::Xcm<RuntimeCall> = message.try_into().unwrap();

		let destination: VersionedLocation = Parachain(SOME_PARA_ID).into();
		let versioned_message: VersionedXcm<RuntimeCall> = VersionedXcm::V4(v4_message.clone());

		let xcm_send_params = IXcm::sendCall {
			destination: destination.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmSendPrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());

		// With V3 it also fails.
		let v3_message: v3::Xcm<RuntimeCall> = v4_message.try_into().unwrap();

		let destination: VersionedLocation = Parachain(SOME_PARA_ID).into();
		let versioned_message: VersionedXcm<RuntimeCall> = VersionedXcm::V3(v3_message);

		let xcm_send_params = IXcm::sendCall {
			destination: destination.encode().into(),
			message: versioned_message.encode().into(),
		};
		let call = IXcm::IXcmCalls::send(xcm_send_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmSendPrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());
	});
}

#[test]
fn test_xcm_execute_precompile_works() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);

		let message: VersionedXcm<RuntimeCall> = VersionedXcm::from(Xcm(vec![
			WithdrawAsset((Here, SEND_AMOUNT).into()),
			buy_execution((Here, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: dest },
		]));

		let weight_params = weighMessageCall { message: message.encode().into() };
		let weight_call = IXcm::IXcmCalls::weighMessage(weight_params);
		let encoded_weight_call = weight_call.abi_encode();

		let xcm_weight_results = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_weight_call,
			&ExecConfig::new_substrate_tx(),
		);

		let weight_result = match xcm_weight_results.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile Failed to decode weight with error {err:?}"),
		};

		let weight: IXcm::Weight = IXcm::Weight::abi_decode(&weight_result.data[..])
			.expect("XcmExecutePrecompile Failed to decode weight");

		let xcm_execute_params = IXcm::executeCall { message: message.encode().into(), weight };
		let call = IXcm::IXcmCalls::execute(xcm_execute_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);

		assert!(result.result.is_ok());
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE - SEND_AMOUNT);
		assert_eq!(Balances::total_balance(&BOB), SEND_AMOUNT);
	});
}

#[test]
fn test_xcm_execute_precompile_different_beneficiary() {
	use codec::Encode;

	let balances = vec![(ALICE, CUSTOM_INITIAL_BALANCE), (CHARLIE, CUSTOM_INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let dest: Location = Junction::AccountId32 { network: None, id: CHARLIE.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);

		let message: VersionedXcm<RuntimeCall> = VersionedXcm::from(Xcm(vec![
			WithdrawAsset((Here, SEND_AMOUNT).into()),
			buy_execution((Here, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: dest },
		]));

		let weight_params = weighMessageCall { message: message.encode().into() };
		let weight_call = IXcm::IXcmCalls::weighMessage(weight_params);
		let encoded_weight_call = weight_call.abi_encode();

		let xcm_weight_results = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_weight_call,
			&ExecConfig::new_substrate_tx(),
		);

		let weight_result = match xcm_weight_results.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile Failed to decode weight with error: {err:?}"),
		};

		let weight: IXcm::Weight = IXcm::Weight::abi_decode(&weight_result.data[..])
			.expect("XcmExecutePrecompile Failed to decode weight");

		let xcm_execute_params = IXcm::executeCall { message: message.encode().into(), weight };
		let call = IXcm::IXcmCalls::execute(xcm_execute_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);

		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile call failed with error: {err:?}"),
		};

		assert!(!return_value.did_revert());
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE - SEND_AMOUNT);
		assert_eq!(Balances::total_balance(&CHARLIE), CUSTOM_INITIAL_BALANCE + SEND_AMOUNT);
	});
}

#[test]
fn test_xcm_execute_precompile_fails() {
	use codec::Encode;

	let balances = vec![(ALICE, CUSTOM_INITIAL_BALANCE), (BOB, CUSTOM_INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);
		let amount_to_send = CUSTOM_INITIAL_BALANCE - ExistentialDeposit::get();
		let assets: Assets = (Here, amount_to_send).into();

		let message: VersionedXcm<RuntimeCall> = VersionedXcm::from(Xcm(vec![
			WithdrawAsset(assets.clone()),
			buy_execution(assets.inner()[0].clone()),
			DepositAsset { assets: assets.clone().into(), beneficiary: dest },
			WithdrawAsset(assets),
		]));

		let weight_params = weighMessageCall { message: message.encode().into() };
		let weight_call = IXcm::IXcmCalls::weighMessage(weight_params);
		let encoded_weight_call = weight_call.abi_encode();

		let xcm_weight_results = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_weight_call,
			&ExecConfig::new_substrate_tx(),
		);

		let weight_result = match xcm_weight_results.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile Failed to decode weight with error: {err:?}"),
		};

		let weight: IXcm::Weight = IXcm::Weight::abi_decode(&weight_result.data[..])
			.expect("XcmExecutePrecompile Failed to decode weight");

		let xcm_execute_params = IXcm::executeCall { message: message.encode().into(), weight };
		let call = IXcm::IXcmCalls::execute(xcm_execute_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);
		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);
		assert_eq!(Balances::total_balance(&BOB), CUSTOM_INITIAL_BALANCE);
	});
}

#[test]
fn execute_fails_on_old_version() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);

		let message = Xcm(vec![
			WithdrawAsset((Here, SEND_AMOUNT).into()),
			buy_execution((Here, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: dest },
		]);
		let versioned_message = VersionedXcm::from(message.clone());

		let weight_params = weighMessageCall { message: versioned_message.encode().into() };
		let weight_call = IXcm::IXcmCalls::weighMessage(weight_params);
		let encoded_weight_call = weight_call.abi_encode();

		let xcm_weight_results = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_weight_call,
			&ExecConfig::new_substrate_tx(),
		);

		let weight_result = match xcm_weight_results.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile Failed to decode weight with error {err:?}"),
		};

		let weight: IXcm::Weight = IXcm::Weight::abi_decode(&weight_result.data[..])
			.expect("XcmExecutePrecompile Failed to decode weight");

		// Using a V4 message to check that it fails.
		let v4_message: v4::Xcm<RuntimeCall> = message.clone().try_into().unwrap();
		let versioned_message = VersionedXcm::V4(v4_message.clone());

		let xcm_execute_params = IXcm::executeCall {
			message: versioned_message.encode().into(),
			weight: weight.clone(),
		};
		let call = IXcm::IXcmCalls::execute(xcm_execute_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);

		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);
		assert_eq!(Balances::total_balance(&BOB), 0);

		// Now using a V3 message.
		let v3_message: v3::Xcm<RuntimeCall> = v4_message.try_into().unwrap();
		let versioned_message = VersionedXcm::V3(v3_message);

		let xcm_execute_params =
			IXcm::executeCall { message: versioned_message.encode().into(), weight };
		let call = IXcm::IXcmCalls::execute(xcm_execute_params);
		let encoded_call = call.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_call,
			&ExecConfig::new_substrate_tx(),
		);

		let return_value = match result.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile call failed with error: {err:?}"),
		};
		assert!(return_value.did_revert());
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);
		assert_eq!(Balances::total_balance(&BOB), 0);
	});
}

#[test]
fn weight_fails_on_old_version() {
	use codec::Encode;

	let balances = vec![
		(ALICE, CUSTOM_INITIAL_BALANCE),
		(ParaId::from(SOME_PARA_ID).into_account_truncating(), CUSTOM_INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), CUSTOM_INITIAL_BALANCE);

		let message: Xcm<RuntimeCall> = Xcm(vec![
			WithdrawAsset((Here, SEND_AMOUNT).into()),
			buy_execution((Here, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: dest },
		]);
		// V4 version is old, fails.
		let v4_message: v4::Xcm<RuntimeCall> = message.try_into().unwrap();
		let versioned_message = VersionedXcm::V4(v4_message.clone());

		let weight_params = weighMessageCall { message: versioned_message.encode().into() };
		let weight_call = IXcm::IXcmCalls::weighMessage(weight_params);
		let encoded_weight_call = weight_call.abi_encode();

		let xcm_weight_results = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_weight_call,
			&ExecConfig::new_substrate_tx(),
		);

		let result = match xcm_weight_results.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile Failed to decode weight with error {err:?}"),
		};
		assert!(result.did_revert());

		// Now we also try V3.
		let v3_message: v3::Xcm<RuntimeCall> = v4_message.try_into().unwrap();
		let versioned_message = VersionedXcm::V3(v3_message);

		let weight_params = weighMessageCall { message: versioned_message.encode().into() };
		let weight_call = IXcm::IXcmCalls::weighMessage(weight_params);
		let encoded_weight_call = weight_call.abi_encode();

		let xcm_weight_results = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			xcm_precompile_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			encoded_weight_call,
			&ExecConfig::new_substrate_tx(),
		);

		let result = match xcm_weight_results.result {
			Ok(value) => value,
			Err(err) => panic!("XcmExecutePrecompile Failed to decode weight with error {err:?}"),
		};
		assert!(result.did_revert());
	});
}

alloy::sol! {
	interface ICaller {
		function delegate(address callee, bytes data, uint64 gas) external returns (bool success, bytes output);
	}
}

/// The delegatecall guard rejects all calls via delegatecall.
#[test]
fn delegatecall_is_rejected() {
	let balances = vec![(ALICE, 1_000_000_000_000_000u128)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let xcm_precompile_addr = H160::from(
			hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap(),
		);

		let (init_code, _) = pallet_revive_fixtures::compile_module_with_type(
			"Caller",
			pallet_revive_fixtures::FixtureType::Solc,
		)
		.expect("Caller fixture must be compiled");
		let caller_addr = pallet_revive::Pallet::<Test>::bare_instantiate(
			RuntimeOrigin::signed(ALICE),
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

		let calldata = ICaller::delegateCall {
			callee: alloy::primitives::Address::from(xcm_precompile_addr.0),
			data: IXcm::weighMessageCall { message: Default::default() }.abi_encode().into(),
			gas: u64::MAX,
		}
		.abi_encode();

		let result = pallet_revive::Pallet::<Test>::bare_call(
			RuntimeOrigin::signed(ALICE),
			caller_addr,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: u128::MAX,
			},
			calldata,
			&ExecConfig::new_substrate_tx(),
		)
		.result
		.expect("outer call must succeed");

		let ret = ICaller::delegateCall::abi_decode_returns(&result.data)
			.expect("return must decode as (bool, bytes)");
		assert!(!ret.success, "DELEGATECALL to XCM precompile must be rejected");
		// PrecompileDelegateDenied is an Error::Error (trap), not an Error::Revert, so the
		// inner call produces no output data. A Solidity-level revert would include
		// ABI-encoded reason bytes.
		assert!(
			ret.output.is_empty(),
			"expected empty output from PrecompileDelegateDenied trap, got {} bytes",
			ret.output.len(),
		);
	});
}

/// Large in bytes but a single instruction, so that under the mock's `FixedWeightBounds` only
/// the per-byte charge differs between this and a small message.
fn one_big_instruction() -> Xcm<()> {
	Xcm(vec![ExpectPallet {
		index: 0,
		name: vec![0u8; 4096],
		module_name: vec![],
		crate_major: 0,
		min_crate_minor: 0,
	}])
}

fn xcm_precompile_address() -> H160 {
	H160::from(hex::const_decode_to_array(b"00000000000000000000000000000000000A0000").unwrap())
}

fn precompile_weight_consumed(calldata: Vec<u8>) -> Weight {
	pallet_revive::Pallet::<Test>::bare_call(
		RuntimeOrigin::signed(ALICE),
		xcm_precompile_address(),
		U256::zero(),
		TransactionLimits::WeightAndDeposit { weight_limit: Weight::MAX, deposit_limit: u128::MAX },
		calldata,
		&ExecConfig::new_substrate_tx(),
	)
	.weight_consumed
}

/// Asserts that the weight charged for `calldata_for(message)` grows with the size of `message`,
/// by at least the per-byte slope `TestWeightInfo` declares (100_000 ps per byte).
fn assert_charge_scales_with_size(calldata_for: impl Fn(Xcm<()>) -> Vec<u8>) {
	use codec::Encode;

	let encoded_len = |message: Xcm<()>| VersionedXcm::from(message).encode().len();
	let (small, big) = (encoded_len(Xcm(vec![ClearOrigin])), encoded_len(one_big_instruction()));
	assert!(big > small + 1_000, "test needs a meaningfully bigger blob: {big} vs {small}");

	let small_weight = precompile_weight_consumed(calldata_for(Xcm(vec![ClearOrigin])));
	let big_weight = precompile_weight_consumed(calldata_for(one_big_instruction()));

	let expected_gap = (big - small) as u64 * 100_000;
	assert!(
		big_weight.ref_time().saturating_sub(small_weight.ref_time()) >= expected_gap,
		"per-byte charge missing or too small: gap {} < expected {expected_gap}",
		big_weight.ref_time().saturating_sub(small_weight.ref_time()),
	);
}

/// `weighMessage` must charge for the size of the blob it decodes and weighs, not a flat fee:
/// the instruction cap does not bound that work.
#[test]
fn weigh_message_charge_scales_with_message_size() {
	use codec::Encode;

	new_test_ext_with_balances(vec![(ALICE, CUSTOM_INITIAL_BALANCE)]).execute_with(|| {
		assert_charge_scales_with_size(|message| {
			IXcm::IXcmCalls::weighMessage(weighMessageCall {
				message: VersionedXcm::from(message).encode().into(),
			})
			.abi_encode()
		});
	});
}

/// `execute` weighs the blob too, and `adjust_gas` must not refund that charge. `max_weight` of
/// zero makes execution fail, so its own charge is fully refunded and only this one remains.
#[test]
fn execute_decode_charge_is_not_refunded() {
	use codec::Encode;

	new_test_ext_with_balances(vec![(ALICE, CUSTOM_INITIAL_BALANCE)]).execute_with(|| {
		assert_charge_scales_with_size(|message| {
			IXcm::IXcmCalls::execute(IXcm::executeCall {
				message: VersionedXcm::from(message).encode().into(),
				weight: IXcm::Weight { refTime: 0, proofSize: 0 },
			})
			.abi_encode()
		});
	});
}
