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

/// These tests exercise the executive layer.
///
/// In these tests the VM/loader are mocked. Instead of dealing with vm bytecode they use
/// simple closures. This allows you to tackle executive logic more thoroughly without writing
/// a VM binary code.
use super::*;
use crate::{
	AddressMapper, Error, Pallet, ReentrancyProtection,
	access_list::{MAX_ACCESS_LIST_ENTRIES, MAX_INLINE_KEY_LEN, StorageOp, Warmth},
	exec::ExportedFunction::*,
	metering::TransactionMeter,
	test_utils::*,
	tests::{
		ExtBuilder, RuntimeEvent as MetaEvent, Test,
		test_utils::{get_balance, place_contract, set_balance},
	},
};
use assert_matches::assert_matches;
use frame_support::{assert_err, assert_ok, parameter_types};
use frame_system::AccountInfo;
use pallet_revive_uapi::ReturnFlags;
use pretty_assertions::assert_eq;
use sp_io::hashing::keccak_256;
use sp_runtime::DispatchError;
use std::{cell::RefCell, collections::hash_map::HashMap, rc::Rc};

type System = frame_system::Pallet<Test>;

type MockStack<'a> = Stack<'a, Test, MockExecutable>;

parameter_types! {
	static Loader: MockLoader = MockLoader::default();
}

fn events() -> Vec<Event<Test>> {
	System::events()
		.into_iter()
		.filter_map(|meta| match meta.event {
			MetaEvent::Contracts(contract_event) => Some(contract_event),
			_ => None,
		})
		.collect()
}

struct MockCtx<'a> {
	ext: &'a mut MockStack<'a>,
	input_data: Vec<u8>,
}

#[derive(Clone)]
struct MockExecutable {
	func: Rc<dyn for<'a> Fn(MockCtx<'a>, &Self) -> ExecResult + 'static>,
	constructor: Rc<dyn for<'a> Fn(MockCtx<'a>, &Self) -> ExecResult + 'static>,
	code_hash: H256,
	code_info: CodeInfo<Test>,
}

#[derive(Default, Clone)]
pub struct MockLoader {
	map: HashMap<H256, MockExecutable>,
	counter: u64,
}

impl MockLoader {
	fn code_hashes() -> Vec<H256> {
		Loader::get().map.keys().copied().collect()
	}

	fn insert(
		func_type: ExportedFunction,
		f: impl Fn(MockCtx, &MockExecutable) -> ExecResult + 'static,
	) -> H256 {
		Loader::mutate(|loader| {
			// Generate code hashes from contract index value.
			let hash = H256(keccak_256(&loader.counter.to_le_bytes()));
			loader.counter += 1;
			if func_type == ExportedFunction::Constructor {
				loader.map.insert(
					hash,
					MockExecutable {
						func: Rc::new(|_, _| exec_success()),
						constructor: Rc::new(f),
						code_hash: hash,
						code_info: CodeInfo::<Test>::new(ALICE),
					},
				);
			} else {
				loader.map.insert(
					hash,
					MockExecutable {
						func: Rc::new(f),
						constructor: Rc::new(|_, _| exec_success()),
						code_hash: hash,
						code_info: CodeInfo::<Test>::new(ALICE),
					},
				);
			}
			hash
		})
	}

	fn insert_both(
		constructor: impl Fn(MockCtx, &MockExecutable) -> ExecResult + 'static,
		call: impl Fn(MockCtx, &MockExecutable) -> ExecResult + 'static,
	) -> H256 {
		Loader::mutate(|loader| {
			// Generate code hashes from contract index value.
			let hash = H256(keccak_256(&loader.counter.to_le_bytes()));
			loader.counter += 1;
			loader.map.insert(
				hash,
				MockExecutable {
					func: Rc::new(call),
					constructor: Rc::new(constructor),
					code_hash: hash,
					code_info: CodeInfo::<Test>::new(ALICE),
				},
			);
			hash
		})
	}
}

impl Executable<Test> for MockExecutable {
	fn from_storage<S: State>(
		code_hash: H256,
		_meter: &mut ResourceMeter<Test, S>,
	) -> Result<Self, DispatchError> {
		Loader::mutate(|loader| {
			loader.map.get(&code_hash).cloned().ok_or(Error::<Test>::CodeNotFound.into())
		})
	}

	fn from_evm_init_code(
		_code: Vec<u8>,
		_owner: AccountIdOf<Test>,
	) -> Result<Self, DispatchError> {
		unimplemented!()
	}

	fn execute<E: Ext<T = Test>>(
		self,
		ext: &mut E,
		function: ExportedFunction,
		input_data: Vec<u8>,
	) -> ExecResult {
		// # Safety
		//
		// We know that we **always** call execute with a `MockStack` in this test.
		//
		// # Note
		//
		// The transmute is necessary because `execute` has to be generic over all
		// `E: Ext`. However, `MockExecutable` can't be generic over `E` as it would
		// constitute a cycle.
		let ext = unsafe { mem::transmute(ext) };
		if function == ExportedFunction::Constructor {
			(self.constructor)(MockCtx { ext, input_data }, &self)
		} else {
			(self.func)(MockCtx { ext, input_data }, &self)
		}
	}

	fn code(&self) -> &[u8] {
		// The mock executable doesn't have code", so we return the code hash.
		self.code_hash.as_ref()
	}

	fn code_hash(&self) -> &H256 {
		&self.code_hash
	}

	fn code_info(&self) -> &CodeInfo<Test> {
		&self.code_info
	}
}

fn exec_success() -> ExecResult {
	Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
}

fn exec_trapped() -> ExecResult {
	Err(ExecError { error: <Error<Test>>::ContractTrapped.into(), origin: ErrorOrigin::Callee })
}

#[test]
fn it_works() {
	parameter_types! {
		static TestData: Vec<usize> = vec![0];
	}

	let value = 0;
	let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

	let exec_ch = MockLoader::insert(Call, |_ctx, _executable| {
		TestData::mutate(|data| data.push(1));
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, exec_ch);

		assert_matches!(
			MockStack::run_call(
				Origin::from_account_id(ALICE),
				BOB_ADDR,
				&mut meter,
				value.into(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			),
			Ok(_)
		);
	});

	assert_eq!(TestData::get(), vec![0, 1]);
}

#[test]
fn transfer_works() {
	// This test verifies that a contract is able to transfer
	// some funds to another account.
	ExtBuilder::default().build().execute_with(|| {
		set_balance(&ALICE, 100);
		set_balance(&BOB, 0);

		let value = 55;
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(Default::default(), u128::MAX).unwrap();

		MockStack::transfer(
			&origin,
			&ALICE,
			&BOB,
			Pallet::<Test>::convert_native_to_evm(value),
			Preservation::Preserve,
			&mut meter,
			&ExecConfig::new_substrate_tx(),
		)
		.unwrap();

		let min_balance = <Test as Config>::Currency::minimum_balance();
		assert!(min_balance > 0);
		assert_eq!(get_balance(&ALICE), 100 - value - min_balance);
		assert_eq!(get_balance(&BOB), min_balance + value);
		assert_eq!(
			meter
				.execute_postponed_deposits(
					&Origin::from_account_id(ALICE),
					&ExecConfig::new_substrate_tx()
				)
				.unwrap(),
			StorageDeposit::Charge(min_balance)
		);
	});
}

#[test]
fn transfer_to_nonexistent_account_works() {
	// This test verifies that a contract is able to transfer
	// some funds to a nonexistent account and that those transfers
	// are not able to reap accounts.
	ExtBuilder::default().build().execute_with(|| {
		let ed = <Test as Config>::Currency::minimum_balance();
		let value = 1024;
		let evm_value = Pallet::<Test>::convert_native_to_evm(value);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(Default::default(), u128::MAX).unwrap();

		// Transfers to nonexistent accounts should work
		set_balance(&ALICE, ed * 2);
		set_balance(&BOB, ed + value);

		assert_ok!(MockStack::transfer(
			&Origin::from_account_id(ALICE),
			&BOB,
			&CHARLIE,
			evm_value,
			Preservation::Preserve,
			&mut meter,
			&ExecConfig::new_substrate_tx(),
		));
		assert_eq!(get_balance(&ALICE), ed);
		assert_eq!(get_balance(&BOB), ed);
		assert_eq!(get_balance(&CHARLIE), ed + value);

		// Do not reap the origin account
		set_balance(&ALICE, ed);
		set_balance(&BOB, ed + value);
		assert_err!(
			MockStack::transfer(
				&Origin::from_account_id(ALICE),
				&BOB,
				&DJANGO,
				evm_value,
				Preservation::Preserve,
				&mut meter,
				&ExecConfig::new_substrate_tx(),
			),
			<Error<Test>>::StorageDepositNotEnoughFunds,
		);

		// Do not reap the sender account
		set_balance(&ALICE, ed * 2);
		set_balance(&BOB, value);
		assert_err!(
			MockStack::transfer(
				&Origin::from_account_id(ALICE),
				&BOB,
				&EVE,
				evm_value,
				Preservation::Preserve,
				&mut meter,
				&ExecConfig::new_substrate_tx(),
			),
			<Error<Test>>::TransferFailed
		);
		// The ED transfer would work. But it should only be executed with the actual transfer
		assert!(!System::account_exists(&EVE));

		assert_eq!(
			meter
				.execute_postponed_deposits(
					&Origin::from_account_id(ALICE),
					&ExecConfig::new_substrate_tx()
				)
				.unwrap(),
			StorageDeposit::Charge(ed),
			"only the successful transfer should charge the ED storage deposit",
		);
	});
}

#[test]
fn correct_transfer_on_call() {
	let value = 55;
	let evm_value = Pallet::<Test>::convert_native_to_evm(value);

	let success_ch = MockLoader::insert(Call, move |ctx, _| {
		assert_eq!(ctx.ext.value_transferred(), evm_value);
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, success_ch);
		set_balance(&ALICE, 100);
		let balance = get_balance(&BOB_FALLBACK);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let _ = MockStack::run_call(
			origin.clone(),
			BOB_ADDR,
			&mut meter,
			evm_value.as_u64().into(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		)
		.unwrap();

		assert_eq!(get_balance(&ALICE), 100 - value);
		assert_eq!(get_balance(&BOB_FALLBACK), balance + value);
	});
}

#[test]
fn correct_transfer_on_delegate_call() {
	let value = 35;
	let evm_value = Pallet::<Test>::convert_native_to_evm(value);

	let success_ch = MockLoader::insert(Call, move |ctx, _| {
		assert_eq!(ctx.ext.value_transferred(), evm_value);
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
	});

	let delegate_ch = MockLoader::insert(Call, move |ctx, _| {
		assert_eq!(ctx.ext.value_transferred(), evm_value);
		ctx.ext.delegate_call(&Default::default(), CHARLIE_ADDR, Vec::new())?;
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, delegate_ch);
		place_contract(&CHARLIE, success_ch);
		set_balance(&ALICE, 100);
		let balance = get_balance(&BOB_FALLBACK);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			evm_value.as_u64().into(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));

		assert_eq!(get_balance(&ALICE), 100 - value);
		assert_eq!(get_balance(&BOB_FALLBACK), balance + value);
	});
}

#[test]
fn delegate_call_missing_contract() {
	let missing_ch = MockLoader::insert(Call, move |_ctx, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
	});

	let delegate_ch = MockLoader::insert(Call, move |ctx, _| {
		ctx.ext.delegate_call(&Default::default(), CHARLIE_ADDR, Vec::new())?;
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: Vec::new() })
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, delegate_ch);
		set_balance(&ALICE, 100);

		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		// contract code missing should still succeed to mimic EVM behavior.
		assert_ok!(MockStack::run_call(
			origin.clone(),
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));

		// add missing contract code
		place_contract(&CHARLIE, missing_ch);
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn changes_are_reverted_on_failing_call() {
	// This test verifies that changes are reverted on a call which fails (or equally, returns
	// a non-zero status code).

	let return_ch = MockLoader::insert(Call, |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: Vec::new() })
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, return_ch);
		set_balance(&ALICE, 100);
		let balance = get_balance(&BOB);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let output = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			55u64.into(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		)
		.unwrap();

		assert!(output.did_revert());
		assert_eq!(get_balance(&ALICE), 100);
		assert_eq!(get_balance(&BOB), balance);
	});
}

#[test]
fn balance_too_low() {
	// This test verifies that a contract can't send value if it's
	// balance is too low.
	let from = BOB;
	let dest = CHARLIE;

	ExtBuilder::default().build().execute_with(|| {
		let ed = <Test as Config>::Currency::minimum_balance();
		set_balance(&ALICE, ed * 2);
		set_balance(&from, ed + 99);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(Default::default(), u128::MAX).unwrap();

		let result = MockStack::transfer(
			&Origin::from_account_id(ALICE),
			&from,
			&dest,
			Pallet::<Test>::convert_native_to_evm(100u128).as_u64().into(),
			Preservation::Preserve,
			&mut meter,
			&ExecConfig::new_substrate_tx(),
		);

		assert_eq!(result, Err(Error::<Test>::TransferFailed.into()));
		assert_eq!(get_balance(&ALICE), ed * 2);
		assert_eq!(get_balance(&from), ed + 99);
		assert_eq!(get_balance(&dest), 0);
	});
}

#[test]
fn output_is_returned_on_success() {
	// Verifies that if a contract returns data with a successful exit status, this data
	// is returned from the execution context.
	let return_ch = MockLoader::insert(Call, |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: vec![1, 2, 3, 4] })
	});

	ExtBuilder::default().build().execute_with(|| {
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		place_contract(&BOB, return_ch);

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);

		let output = result.unwrap();
		assert!(!output.did_revert());
		assert_eq!(output.data, vec![1, 2, 3, 4]);
	});
}

#[test]
fn output_is_returned_on_failure() {
	// Verifies that if a contract returns data with a failing exit status, this data
	// is returned from the execution context.
	let return_ch = MockLoader::insert(Call, |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![1, 2, 3, 4] })
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, return_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);

		let output = result.unwrap();
		assert!(output.did_revert());
		assert_eq!(output.data, vec![1, 2, 3, 4]);
	});
}

#[test]
fn input_data_to_call() {
	let input_data_ch = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(ctx.input_data, &[1, 2, 3, 4]);
		exec_success()
	});

	// This one tests passing the input data into a contract via call.
	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, input_data_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![1, 2, 3, 4],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn input_data_to_instantiate() {
	let input_data_ch = MockLoader::insert(Constructor, |ctx, _| {
		assert_eq!(ctx.input_data, &[1, 2, 3, 4]);
		exec_success()
	});

	// This one tests passing the input data into a contract via instantiate.
	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
					.unwrap();

			let executable = MockExecutable::from_storage(input_data_ch, &mut meter).unwrap();
			set_balance(&ALICE, min_balance * 10_000);

			let result = MockStack::run_instantiate(
				ALICE,
				executable,
				&mut meter,
				min_balance.into(),
				vec![1, 2, 3, 4],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			);
			assert_matches!(result, Ok(_));
		});
}

#[test]
fn max_depth() {
	// This test verifies that when we reach the maximal depth creation of an
	// yet another context fails.
	parameter_types! {
		static ReachedBottom: bool = false;
	}
	let value = 0;
	let recurse_ch = MockLoader::insert(Call, |ctx, _| {
		// Try to call into yourself.
		let r = ctx.ext.call(
			&Default::default(),
			&BOB_ADDR,
			U256::zero(),
			vec![],
			ReentrancyProtection::AllowReentry,
			false,
		);

		ReachedBottom::mutate(|reached_bottom| {
			if !*reached_bottom {
				// We are first time here, it means we just reached bottom.
				// Verify that we've got proper error and set `reached_bottom`.
				assert_eq!(r, Err(Error::<Test>::MaxCallDepthReached.into()));
				*reached_bottom = true;
			} else {
				// We just unwinding stack here.
				assert_matches!(r, Ok(_));
			}
		});

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		set_balance(&BOB, 1);
		place_contract(&BOB, recurse_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			value.into(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);

		assert_matches!(result, Ok(_));
	});
}

#[test]
fn caller_returns_proper_values() {
	parameter_types! {
		static WitnessedCallerBob: Option<H160> = None;
		static WitnessedCallerCharlie: Option<H160> = None;
	}

	let bob_ch = MockLoader::insert(Call, |ctx, _| {
		// Record the caller for bob.
		WitnessedCallerBob::mutate(|caller| {
			let origin = ctx.ext.caller();
			*caller = Some(<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_address(
				&origin.account_id().unwrap(),
			));
		});

		// Call into CHARLIE contract.
		assert_matches!(
			ctx.ext.call(
				&Default::default(),
				&CHARLIE_ADDR,
				U256::zero(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false
			),
			Ok(_)
		);
		exec_success()
	});
	let charlie_ch = MockLoader::insert(Call, |ctx, _| {
		// Record the caller for charlie.
		WitnessedCallerCharlie::mutate(|caller| {
			let origin = ctx.ext.caller();
			*caller = Some(<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_address(
				&origin.account_id().unwrap(),
			));
		});
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, bob_ch);
		place_contract(&CHARLIE, charlie_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);

		assert_matches!(result, Ok(_));
	});

	assert_eq!(WitnessedCallerBob::get(), Some(ALICE_ADDR));
	assert_eq!(WitnessedCallerCharlie::get(), Some(BOB_ADDR));
}

#[test]
fn origin_returns_proper_values() {
	parameter_types! {
		static WitnessedCallerBob: Option<H160> = None;
		static WitnessedCallerCharlie: Option<H160> = None;
	}

	let bob_ch = MockLoader::insert(Call, |ctx, _| {
		// Record the origin for bob.
		WitnessedCallerBob::mutate(|witness| {
			let origin = ctx.ext.origin();
			*witness =
				Some(<Test as Config>::AddressMapper::to_address(&origin.account_id().unwrap()));
		});

		// Call into CHARLIE contract.
		assert_matches!(
			ctx.ext.call(
				&Default::default(),
				&CHARLIE_ADDR,
				U256::zero(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false
			),
			Ok(_)
		);
		exec_success()
	});
	let charlie_ch = MockLoader::insert(Call, |ctx, _| {
		// Record the origin for charlie.
		WitnessedCallerCharlie::mutate(|witness| {
			let origin = ctx.ext.origin();
			*witness =
				Some(<Test as Config>::AddressMapper::to_address(&origin.account_id().unwrap()));
		});
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, bob_ch);
		place_contract(&CHARLIE, charlie_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);

		assert_matches!(result, Ok(_));
	});

	assert_eq!(WitnessedCallerBob::get(), Some(ALICE_ADDR));
	assert_eq!(WitnessedCallerCharlie::get(), Some(ALICE_ADDR));
}

#[test]
fn to_account_id_returns_proper_values() {
	let bob_code_hash = MockLoader::insert(Call, |ctx, _| {
		let alice_account_id = <Test as Config>::AddressMapper::to_account_id(&ALICE_ADDR);
		assert_eq!(ctx.ext.to_account_id(&ALICE_ADDR), alice_account_id);

		const UNMAPPED_ADDR: H160 = H160([99u8; 20]);
		let mut unmapped_fallback_account_id = [0xEE; 32];
		unmapped_fallback_account_id[..20].copy_from_slice(UNMAPPED_ADDR.as_bytes());
		assert_eq!(
			ctx.ext.to_account_id(&UNMAPPED_ADDR),
			AccountId32::new(unmapped_fallback_account_id)
		);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, bob_code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn code_hash_returns_proper_values() {
	let bob_code_hash = MockLoader::insert(Call, |ctx, _| {
		// ALICE is not a contract but account exists so it returns hash of empty data
		assert_eq!(ctx.ext.code_hash(&ALICE_ADDR), EMPTY_CODE_HASH);
		// BOB is a contract (this function) and hence it has a code_hash.
		// `MockLoader` uses contract index to generate the code hash.
		assert_eq!(ctx.ext.code_hash(&BOB_ADDR), H256(keccak_256(&0u64.to_le_bytes())));
		// [0xff;20] doesn't exist and returns hash zero
		assert!(ctx.ext.code_hash(&H160([0xff; 20])).is_zero());

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		// add alice account info to test case EOA code hash
		frame_system::Account::<Test>::insert(
			<Test as Config>::AddressMapper::to_account_id(&ALICE_ADDR),
			AccountInfo { consumers: 1, providers: 1, ..Default::default() },
		);
		place_contract(&BOB, bob_code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		// ALICE (not contract) -> BOB (contract)
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn own_code_hash_returns_proper_values() {
	let bob_ch = MockLoader::insert(Call, |ctx, _| {
		let code_hash = ctx.ext.code_hash(&BOB_ADDR);
		assert_eq!(*ctx.ext.own_code_hash(), code_hash);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, bob_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		// ALICE (not contract) -> BOB (contract)
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn caller_is_origin_returns_proper_values() {
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		// BOB is not the origin of the stack call
		assert!(!ctx.ext.caller_is_origin(false));
		exec_success()
	});

	let code_bob = MockLoader::insert(Call, |ctx, _| {
		// ALICE is the origin of the call stack
		assert!(ctx.ext.caller_is_origin(false));
		// BOB calls CHARLIE
		ctx.ext
			.call(
				&Default::default(),
				&CHARLIE_ADDR,
				U256::zero(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false,
			)
			.map(|_| ctx.ext.last_frame_output().clone())
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		// ALICE -> BOB (caller is origin) -> CHARLIE (caller is not origin)
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn root_caller_succeeds() {
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		// root is the origin of the call stack.
		assert!(ctx.ext.caller_is_root(false));
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		let origin = Origin::Root;
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		// root -> BOB (caller is root)
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn root_caller_does_not_succeed_when_value_not_zero() {
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		// root is the origin of the call stack.
		assert!(ctx.ext.caller_is_root(false));
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		let origin = Origin::Root;
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		// root -> BOB (caller is root)
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			1u64.into(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Err(_));
	});
}

#[test]
fn root_caller_succeeds_with_consecutive_calls() {
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		// BOB is not root, even though the origin is root.
		assert!(!ctx.ext.caller_is_root(false));
		exec_success()
	});

	let code_bob = MockLoader::insert(Call, |ctx, _| {
		// root is the origin of the call stack.
		assert!(ctx.ext.caller_is_root(false));
		// BOB calls CHARLIE.
		ctx.ext
			.call(
				&Default::default(),
				&CHARLIE_ADDR,
				U256::zero(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false,
			)
			.map(|_| ctx.ext.last_frame_output().clone())
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::Root;
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		// root -> BOB (caller is root) -> CHARLIE (caller is not root)
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn address_returns_proper_values() {
	let bob_ch = MockLoader::insert(Call, |ctx, _| {
		// Verify that address matches BOB.
		assert_eq!(ctx.ext.address(), BOB_ADDR);

		// Call into charlie contract.
		assert_matches!(
			ctx.ext.call(
				&Default::default(),
				&CHARLIE_ADDR,
				U256::zero(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false
			),
			Ok(_)
		);
		exec_success()
	});
	let charlie_ch = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(ctx.ext.address(), CHARLIE_ADDR);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, bob_ch);
		place_contract(&CHARLIE, charlie_ch);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);

		assert_matches!(result, Ok(_));
	});
}

#[test]
fn refuse_instantiate_with_value_below_existential_deposit() {
	let dummy_ch = MockLoader::insert(Constructor, |_, _| exec_success());

	ExtBuilder::default().existential_deposit(15).build().execute_with(|| {
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		let executable = MockExecutable::from_storage(dummy_ch, &mut meter).unwrap();

		assert_matches!(
			MockStack::run_instantiate(
				ALICE,
				executable,
				&mut meter,
				U256::zero(), // <- zero value
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			),
			Err(_)
		);
	});
}

#[test]
fn instantiation_work_with_success_output() {
	let dummy_ch = MockLoader::insert(Constructor, |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: vec![80, 65, 83, 83] })
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			set_balance(&ALICE, min_balance * 1000);
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, min_balance * 100).unwrap();
			let executable = MockExecutable::from_storage(dummy_ch, &mut meter).unwrap();

			let instantiated_contract_address = assert_matches!(
				MockStack::run_instantiate(
					ALICE,
					executable,
					&mut meter,
					Pallet::<Test>::convert_native_to_evm(min_balance),
					vec![],
					Some(&[0 ;32]),
					&ExecConfig::new_substrate_tx(),
				),
				Ok((address, ref output)) if output.data == vec![80, 65, 83, 83] => address
			);
			let instantiated_contract_id =
				<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_fallback_account_id(
					&instantiated_contract_address,
				);

			// Check that the newly created account has the expected code hash and
			// there are instantiation event.
			assert_eq!(
				ContractInfo::<Test>::load_code_hash(&instantiated_contract_id).unwrap(),
				dummy_ch
			);
			assert_eq!(
				&events(),
				&[Event::Instantiated {
					deployer: ALICE_ADDR,
					contract: instantiated_contract_address
				}]
			);
		});
}

#[test]
fn instantiation_fails_with_failing_output() {
	let dummy_ch = MockLoader::insert(Constructor, |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![70, 65, 73, 76] })
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, min_balance * 100).unwrap();
			let executable = MockExecutable::from_storage(dummy_ch, &mut meter).unwrap();
			set_balance(&ALICE, min_balance * 1000);

			let instantiated_contract_address = assert_matches!(
				MockStack::run_instantiate(
					ALICE,
					executable,
					&mut meter,
					Pallet::<Test>::convert_native_to_evm(min_balance),
					vec![],
					Some(&[0; 32]),
					&ExecConfig::new_substrate_tx(),
				),
				Ok((address, ref output)) if output.data == vec![70, 65, 73, 76] => address
			);

			let instantiated_contract_id =
				<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_fallback_account_id(
					&instantiated_contract_address,
				);

			// Check that the account has not been created.
			assert!(ContractInfo::<Test>::load_code_hash(&instantiated_contract_id).is_none());
			assert!(events().is_empty());
		});
}

#[test]
fn instantiation_from_contract() {
	let dummy_ch = MockLoader::insert(Call, |_, _| exec_success());
	let instantiated_contract_address = Rc::new(RefCell::new(None::<H160>));
	let instantiator_ch = MockLoader::insert(Call, {
		let instantiated_contract_address = Rc::clone(&instantiated_contract_address);
		move |ctx, _| {
			// Instantiate a contract and save it's address in `instantiated_contract_address`.
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let (address, output) = ctx
				.ext
				.instantiate(
					&CallResources::NoLimits,
					Code::Existing(dummy_ch),
					Pallet::<Test>::convert_native_to_evm(min_balance),
					vec![],
					Some(&[48; 32]),
				)
				.map(|address| (address, ctx.ext.last_frame_output().clone()))
				.unwrap();

			*instantiated_contract_address.borrow_mut() = Some(address);
			Ok(output)
		}
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			set_balance(&ALICE, min_balance * 100);
			place_contract(&BOB, instantiator_ch);
			let origin = Origin::from_account_id(ALICE);
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, min_balance * 10).unwrap();

			assert_matches!(
				MockStack::run_call(
					origin,
					BOB_ADDR,
					&mut meter,
					Pallet::<Test>::convert_native_to_evm(min_balance * 10),
					vec![],
					&ExecConfig::new_substrate_tx(),
				),
				Ok(_)
			);

			let instantiated_contract_address =
				*instantiated_contract_address.borrow().as_ref().unwrap();

			let instantiated_contract_id =
				<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_fallback_account_id(
					&instantiated_contract_address,
				);

			// Check that the newly created account has the expected code hash and
			// there are instantiation event.
			assert_eq!(
				ContractInfo::<Test>::load_code_hash(&instantiated_contract_id).unwrap(),
				dummy_ch
			);
		});
}

#[test]
fn reentrant_instantiate_at_same_address_is_rejected() {
	// EIP-684: while `B1` constructs at address `X`, its constructor re-enters the deployer to
	// instantiate the same code+salt. That resolves to `X` again and must be rejected rather
	// than run a second constructor for one account.
	let salt = [42u8; 32];

	let constructor_ch = MockLoader::insert(Constructor, |ctx, _| {
		// Re-enter the deployer (BOB) while we are still being constructed.
		ctx.ext
			.call(
				&CallResources::NoLimits,
				&BOB_ADDR,
				U256::zero(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false,
			)
			.unwrap();
		exec_success()
	});

	let invocations = Rc::new(RefCell::new(0u32));
	let second_instantiate_error = Rc::new(RefCell::new(None::<DispatchError>));
	let factory_ch = MockLoader::insert(Call, {
		let invocations = Rc::clone(&invocations);
		let second_instantiate_error = Rc::clone(&second_instantiate_error);
		move |ctx, _| {
			*invocations.borrow_mut() += 1;
			let n = *invocations.borrow();
			// Bound the recursion in case the guard fails to reject the collision.
			if n <= 2 {
				let min_balance = <Test as Config>::Currency::minimum_balance();
				let value = Pallet::<Test>::convert_native_to_evm(min_balance);
				let result = ctx.ext.instantiate(
					&CallResources::NoLimits,
					Code::Existing(constructor_ch),
					value,
					vec![],
					Some(&salt),
				);
				if n == 2 {
					if let Err(err) = &result {
						*second_instantiate_error.borrow_mut() = Some(err.error);
					}
				}
			}
			exec_success()
		}
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			set_balance(&ALICE, min_balance * 1000);
			place_contract(&BOB, factory_ch);
			let origin = Origin::from_account_id(ALICE);
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, min_balance * 100).unwrap();

			// `B1` still constructs; only the colliding re-entrant instantiate fails.
			assert_ok!(MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				Pallet::<Test>::convert_native_to_evm(min_balance * 100),
				vec![],
				&ExecConfig::new_substrate_tx(),
			));

			// Initial call plus one re-entry; without the guard it would recurse further.
			assert_eq!(*invocations.borrow(), 2);
			assert_eq!(
				*second_instantiate_error.borrow(),
				Some(<Error<Test>>::DuplicateContract.into())
			);
		});
}

#[test]
fn instantiation_traps() {
	let dummy_ch = MockLoader::insert(Constructor, |_, _| Err("It's a trap!".into()));
	let instantiator_ch = MockLoader::insert(Call, {
		move |ctx, _| {
			// Instantiate a contract and save it's address in `instantiated_contract_address`.
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let value = Pallet::<Test>::convert_native_to_evm(min_balance);

			assert_matches!(
				ctx.ext.instantiate(
					&Default::default(),
					Code::Existing(dummy_ch),
					value,
					vec![],
					Some(&[0; 32]),
				),
				Err(ExecError {
					error: DispatchError::Other("It's a trap!"),
					origin: ErrorOrigin::Callee,
				})
			);

			exec_success()
		}
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			set_balance(&ALICE, 1000);
			set_balance(&BOB_FALLBACK, 100);
			place_contract(&BOB, instantiator_ch);
			let origin = Origin::from_account_id(ALICE);
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 200).unwrap();

			assert_matches!(
				MockStack::run_call(
					origin,
					BOB_ADDR,
					&mut meter,
					U256::zero(),
					vec![],
					&ExecConfig::new_substrate_tx(),
				),
				Ok(_)
			);
		});
}

#[test]
fn termination_from_instantiate_succeeds() {
	let terminate_ch = MockLoader::insert(Constructor, |ctx, _| {
		let _ = ctx.ext.terminate_if_same_tx(&ALICE_ADDR)?;
		exec_success()
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
					.unwrap();
			let executable = MockExecutable::from_storage(terminate_ch, &mut meter).unwrap();
			set_balance(&ALICE, 10_000);

			let result = MockStack::run_instantiate(
				ALICE,
				executable,
				&mut meter,
				Pallet::<Test>::convert_native_to_evm(100u128),
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap();

			assert_eq!(
				&events(),
				&[Event::Instantiated { deployer: ALICE_ADDR, contract: result.0 }]
			);
		});
}

#[test]
fn in_memory_changes_not_discarded() {
	// Call stack: BOB -> CHARLIE (trap) -> BOB' (success)
	// This tests verifies some edge case of the contract info cache:
	// We change some value in our contract info before calling into a contract
	// that calls into ourself. This triggers a case where BOBs contract info
	// is written to storage and invalidated by the successful execution of BOB'.
	// The trap of CHARLIE reverts the storage changes to BOB. When the root BOB regains
	// control it reloads its contract info from storage. We check that changes that
	// are made before calling into CHARLIE are not discarded.
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		if ctx.input_data[0] == 0 {
			let info = ctx.ext.contract_info();
			assert_eq!(info.storage_byte_deposit, 0);
			info.storage_byte_deposit = 42;
			assert_eq!(
				ctx.ext
					.call(
						&Default::default(),
						&CHARLIE_ADDR,
						U256::zero(),
						vec![],
						ReentrancyProtection::AllowReentry,
						false
					)
					.map(|_| ctx.ext.last_frame_output().clone()),
				exec_trapped()
			);
			assert_eq!(ctx.ext.contract_info().storage_byte_deposit, 42);
		}
		exec_success()
	});
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		assert!(
			ctx.ext
				.call(
					&Default::default(),
					&BOB_ADDR,
					U256::zero(),
					vec![99],
					ReentrancyProtection::AllowReentry,
					false
				)
				.is_ok()
		);
		exec_trapped()
	});

	// This one tests passing the input data into a contract via call.
	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn bank_after_invalidate_loads_cache_for_refund_pro_rating() {
	// Exercises a banked refund: self-reenter, `charge_storage` a removal (no cache reload),
	// self-reenter again. The second bank fires on an invalidated frame; `load()` reloads so
	// the removal's pro-rata refund is applied. 30 bytes (not 1) so the refund doesn't round
	// to 0.
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		if ctx.input_data[0] == 0 {
			ctx.ext.set_storage(&Key::Fix([1; 32]), Some(vec![1, 2, 3]), false).unwrap();
			assert_ok!(ctx.ext.call(
				&Default::default(),
				&BOB_ADDR,
				U256::zero(),
				vec![1],
				ReentrancyProtection::AllowReentry,
				false,
			));
			ctx.ext
				.charge_storage(&Diff { bytes_removed: 30, ..Default::default() })
				.unwrap();
			assert_ok!(ctx.ext.call(
				&Default::default(),
				&BOB_ADDR,
				U256::zero(),
				vec![1],
				ReentrancyProtection::AllowReentry,
				false,
			));
		}
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();
		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_bob);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		let origin = Origin::from_account_id(ALICE);
		let exec_config = ExecConfig::new_substrate_tx();
		assert_ok!(MockStack::run_call(
			origin.clone(),
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&exec_config,
		));
		meter.execute_postponed_deposits(&origin, &exec_config).unwrap();

		// K1 = 35 bytes → deposit 37; 30-byte removal refunds floor(30/35 * 35) = 29 → net 8.
		let charged = min_balance * 1000 - get_balance(&ALICE);
		assert_eq!(charged, 8, "banked removal refund not applied: expected net deposit 8");
	});
}

#[test]
fn recursive_call_during_constructor_is_balance_transfer() {
	let code = MockLoader::insert(Constructor, |ctx, _| {
		let account_id = ctx.ext.account_id().clone();
		let addr =
			<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_address(&account_id);
		let balance = ctx.ext.balance();

		// Calling ourselves during the constructor will trigger a balance
		// transfer since no contract exist yet.
		assert_ok!(ctx.ext.call(
			&Default::default(),
			&addr,
			(balance - 1).into(),
			vec![],
			ReentrancyProtection::AllowReentry,
			false
		));

		// Should also work with call data set as it is ignored when no
		// contract is deployed.
		assert_ok!(ctx.ext.call(
			&Default::default(),
			&addr,
			1u32.into(),
			vec![1, 2, 3, 4],
			ReentrancyProtection::AllowReentry,
			false
		));
		exec_success()
	});

	// This one tests passing the input data into a contract via instantiate.
	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
					.unwrap();
			let executable = MockExecutable::from_storage(code, &mut meter).unwrap();
			set_balance(&ALICE, min_balance * 10_000);

			let result = MockStack::run_instantiate(
				ALICE,
				executable,
				&mut meter,
				10u64.into(),
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			);
			assert_matches!(result, Ok(_));
		});
}

#[test]
fn cannot_send_more_balance_than_available_to_self() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		let account_id = ctx.ext.account_id().clone();
		let addr =
			<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_address(&account_id);
		let balance = ctx.ext.balance();

		assert_err!(
			ctx.ext.call(
				&Default::default(),
				&addr,
				(balance + 1).into(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false
			),
			<Error<Test>>::TransferFailed,
		);
		exec_success()
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
			set_balance(&ALICE, min_balance * 10);
			place_contract(&BOB, code_hash);
			let origin = Origin::from_account_id(ALICE);
			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap();
		});
}

#[test]
fn call_reentry_direct_recursion() {
	// call the contract passed as input with disabled reentry
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		let dest = H160::from_slice(ctx.input_data.as_ref());
		ctx.ext
			.call(
				&Default::default(),
				&dest,
				U256::zero(),
				vec![],
				ReentrancyProtection::Strict,
				false,
			)
			.map(|_| ctx.ext.last_frame_output().clone())
	});

	let code_charlie = MockLoader::insert(Call, |_, _| exec_success());

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		// Calling another contract should succeed
		assert_ok!(MockStack::run_call(
			origin.clone(),
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			CHARLIE_ADDR.as_bytes().to_vec(),
			&ExecConfig::new_substrate_tx(),
		));

		// Calling into oneself fails
		assert_err!(
			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				BOB_ADDR.as_bytes().to_vec(),
				&ExecConfig::new_substrate_tx(),
			)
			.map_err(|e| e.error),
			<Error<Test>>::ReentranceDenied,
		);
	});
}

#[test]
fn call_deny_reentry() {
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		if ctx.input_data[0] == 0 {
			ctx.ext
				.call(
					&Default::default(),
					&CHARLIE_ADDR,
					U256::zero(),
					vec![],
					ReentrancyProtection::Strict,
					false,
				)
				.map(|_| ctx.ext.last_frame_output().clone())
		} else {
			exec_success()
		}
	});

	// call BOB with input set to '1'
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		ctx.ext
			.call(
				&Default::default(),
				&BOB_ADDR,
				U256::zero(),
				vec![1],
				ReentrancyProtection::AllowReentry,
				false,
			)
			.map(|_| ctx.ext.last_frame_output().clone())
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		// BOB -> CHARLIE -> BOB fails as BOB denies reentry.
		assert_err!(
			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![0],
				&ExecConfig::new_substrate_tx(),
			)
			.map_err(|e| e.error),
			<Error<Test>>::ReentranceDenied,
		);
	});
}

#[test]
fn minimum_balance_must_return_converted_balance() {
	let min_balance: BalanceOf<Test> = <Test as Config>::Currency::minimum_balance();
	let min_balance_evm_value: U256 = Pallet::<Test>::convert_native_to_evm(min_balance);

	let succ_fail_code = MockLoader::insert(Constructor, move |ctx, _| {
		// The value returned by `Ext::minimum_balance` is `U256`, it must
		// have been converted from the native balance type to `U256`.
		assert_eq!(ctx.ext.minimum_balance(), min_balance_evm_value);
		exec_success()
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.build()
		.execute_with(|| {
			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
					.unwrap();
			let succ_fail_executable =
				MockExecutable::from_storage(succ_fail_code, &mut meter).unwrap();
			set_balance(&ALICE, min_balance * 10_000);

			assert_ok!(MockStack::run_instantiate(
				ALICE,
				succ_fail_executable,
				&mut meter,
				min_balance_evm_value,
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			));
		});
}

#[test]
fn nonce() {
	let fail_code = MockLoader::insert(Constructor, |_, _| exec_trapped());
	let success_code = MockLoader::insert(Constructor, |_, _| exec_success());
	let succ_fail_code = MockLoader::insert(Constructor, move |ctx, _| {
		ctx.ext
			.instantiate(
				&CallResources::NoLimits,
				Code::Existing(fail_code),
				ctx.ext.minimum_balance() * 100,
				vec![],
				Some(&[0; 32]),
			)
			.ok();
		exec_success()
	});
	let succ_succ_code = MockLoader::insert(Constructor, move |ctx, _| {
		let alice_nonce = System::account_nonce(&ALICE);
		assert_eq!(System::account_nonce(ctx.ext.account_id()), 1);
		assert_eq!(ctx.ext.caller().account_id().unwrap(), &ALICE);
		let addr = ctx
			.ext
			.instantiate(
				&CallResources::NoLimits,
				Code::Existing(success_code),
				ctx.ext.minimum_balance(),
				vec![],
				Some(&[0; 32]),
			)
			.unwrap();

		let account_id =
			<<Test as Config>::AddressMapper as AddressMapper<Test>>::to_fallback_account_id(&addr);

		assert_eq!(System::account_nonce(&ALICE), alice_nonce);
		assert_eq!(System::account_nonce(ctx.ext.account_id()), 2);
		assert_eq!(System::account_nonce(&account_id), 1);

		// a plain call should not influence the account counter
		ctx.ext
			.call(
				&Default::default(),
				&addr,
				U256::zero(),
				vec![],
				ReentrancyProtection::Strict,
				false,
			)
			.unwrap();

		assert_eq!(System::account_nonce(ALICE), alice_nonce);
		assert_eq!(System::account_nonce(ctx.ext.account_id()), 2);
		assert_eq!(System::account_nonce(&account_id), 1);

		exec_success()
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.build()
		.execute_with(|| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let min_balance_evm_value: U256 = Pallet::<Test>::convert_native_to_evm(min_balance);

			let mut meter =
				TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
					.unwrap();
			let fail_executable = MockExecutable::from_storage(fail_code, &mut meter).unwrap();
			let success_executable =
				MockExecutable::from_storage(success_code, &mut meter).unwrap();
			let succ_fail_executable =
				MockExecutable::from_storage(succ_fail_code, &mut meter).unwrap();
			let succ_succ_executable =
				MockExecutable::from_storage(succ_succ_code, &mut meter).unwrap();
			set_balance(&ALICE, min_balance * 10_000);
			set_balance(&BOB, min_balance * 10_000);

			// fail should not increment
			MockStack::run_instantiate(
				ALICE,
				fail_executable,
				&mut meter,
				min_balance_evm_value * 100,
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			)
			.ok();
			assert_eq!(System::account_nonce(&ALICE), 0);

			assert_ok!(MockStack::run_instantiate(
				ALICE,
				success_executable,
				&mut meter,
				min_balance_evm_value * 100,
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			));
			assert_eq!(System::account_nonce(&ALICE), 1);

			assert_ok!(MockStack::run_instantiate(
				ALICE,
				succ_fail_executable,
				&mut meter,
				min_balance_evm_value * 200,
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			));
			assert_eq!(System::account_nonce(&ALICE), 2);

			assert_ok!(MockStack::run_instantiate(
				ALICE,
				succ_succ_executable,
				&mut meter,
				min_balance_evm_value * 200,
				vec![],
				Some(&[0; 32]),
				&ExecConfig::new_substrate_tx(),
			));
			assert_eq!(System::account_nonce(&ALICE), 3);
		});
}

#[test]
fn set_storage_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		// Write
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([1; 32]), Some(vec![1, 2, 3]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([2; 32]), Some(vec![4, 5, 6]), true),
			Ok(WriteOutcome::New)
		);
		assert_eq!(ctx.ext.set_storage(&Key::Fix([3; 32]), None, false), Ok(WriteOutcome::New));
		assert_eq!(ctx.ext.set_storage(&Key::Fix([4; 32]), None, true), Ok(WriteOutcome::New));
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([5; 32]), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([6; 32]), Some(vec![]), true),
			Ok(WriteOutcome::New)
		);

		// Overwrite
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([1; 32]), Some(vec![42]), false),
			Ok(WriteOutcome::Overwritten(3))
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([2; 32]), Some(vec![48]), true),
			Ok(WriteOutcome::Taken(vec![4, 5, 6]))
		);
		assert_eq!(ctx.ext.set_storage(&Key::Fix([3; 32]), None, false), Ok(WriteOutcome::New));
		assert_eq!(ctx.ext.set_storage(&Key::Fix([4; 32]), None, true), Ok(WriteOutcome::New));
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([5; 32]), Some(vec![]), false),
			Ok(WriteOutcome::Overwritten(0))
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([6; 32]), Some(vec![]), true),
			Ok(WriteOutcome::Taken(vec![]))
		);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();

		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();

		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn set_storage_varsized_key_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		// Write
		assert_eq!(
			ctx.ext.set_storage(
				&Key::try_from_var([1; 64].to_vec()).unwrap(),
				Some(vec![1, 2, 3]),
				false
			),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(
				&Key::try_from_var([2; 19].to_vec()).unwrap(),
				Some(vec![4, 5, 6]),
				true
			),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::try_from_var([3; 19].to_vec()).unwrap(), None, false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::try_from_var([4; 64].to_vec()).unwrap(), None, true),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext
				.set_storage(&Key::try_from_var([5; 30].to_vec()).unwrap(), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext
				.set_storage(&Key::try_from_var([6; 128].to_vec()).unwrap(), Some(vec![]), true),
			Ok(WriteOutcome::New)
		);

		// Overwrite
		assert_eq!(
			ctx.ext.set_storage(
				&Key::try_from_var([1; 64].to_vec()).unwrap(),
				Some(vec![42, 43, 44]),
				false
			),
			Ok(WriteOutcome::Overwritten(3))
		);
		assert_eq!(
			ctx.ext.set_storage(
				&Key::try_from_var([2; 19].to_vec()).unwrap(),
				Some(vec![48]),
				true
			),
			Ok(WriteOutcome::Taken(vec![4, 5, 6]))
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::try_from_var([3; 19].to_vec()).unwrap(), None, false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::try_from_var([4; 64].to_vec()).unwrap(), None, true),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext
				.set_storage(&Key::try_from_var([5; 30].to_vec()).unwrap(), Some(vec![]), false),
			Ok(WriteOutcome::Overwritten(0))
		);
		assert_eq!(
			ctx.ext
				.set_storage(&Key::try_from_var([6; 128].to_vec()).unwrap(), Some(vec![]), true),
			Ok(WriteOutcome::Taken(vec![]))
		);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();

		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn get_storage_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([1; 32]), Some(vec![1, 2, 3]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([2; 32]), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(ctx.ext.get_storage(&Key::Fix([1; 32])), Some(vec![1, 2, 3]));
		assert_eq!(ctx.ext.get_storage(&Key::Fix([2; 32])), Some(vec![]));
		assert_eq!(ctx.ext.get_storage(&Key::Fix([3; 32])), None);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();

		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn get_storage_size_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([1; 32]), Some(vec![1, 2, 3]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_storage(&Key::Fix([2; 32]), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(ctx.ext.get_storage_size(&Key::Fix([1; 32])), Some(3));
		assert_eq!(ctx.ext.get_storage_size(&Key::Fix([2; 32])), Some(0));
		assert_eq!(ctx.ext.get_storage_size(&Key::Fix([3; 32])), None);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();

		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn get_storage_varsized_key_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(
			ctx.ext.set_storage(
				&Key::try_from_var([1; 19].to_vec()).unwrap(),
				Some(vec![1, 2, 3]),
				false
			),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext
				.set_storage(&Key::try_from_var([2; 16].to_vec()).unwrap(), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.get_storage(&Key::try_from_var([1; 19].to_vec()).unwrap()),
			Some(vec![1, 2, 3])
		);
		assert_eq!(
			ctx.ext.get_storage(&Key::try_from_var([2; 16].to_vec()).unwrap()),
			Some(vec![])
		);
		assert_eq!(ctx.ext.get_storage(&Key::try_from_var([3; 8].to_vec()).unwrap()), None);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();

		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn get_storage_size_varsized_key_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(
			ctx.ext.set_storage(
				&Key::try_from_var([1; 19].to_vec()).unwrap(),
				Some(vec![1, 2, 3]),
				false
			),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext
				.set_storage(&Key::try_from_var([2; 16].to_vec()).unwrap(), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.get_storage_size(&Key::try_from_var([1; 19].to_vec()).unwrap()),
			Some(3)
		);
		assert_eq!(
			ctx.ext.get_storage_size(&Key::try_from_var([2; 16].to_vec()).unwrap()),
			Some(0)
		);
		assert_eq!(ctx.ext.get_storage_size(&Key::try_from_var([3; 8].to_vec()).unwrap()), None);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		let min_balance = <Test as Config>::Currency::minimum_balance();

		set_balance(&ALICE, min_balance * 1000);
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn set_transient_storage_works() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		// Write
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([1; 32]), Some(vec![1, 2, 3]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([2; 32]), Some(vec![4, 5, 6]), true),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([3; 32]), None, false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([4; 32]), None, true),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([5; 32]), Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([6; 32]), Some(vec![]), true),
			Ok(WriteOutcome::New)
		);

		// Overwrite
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([1; 32]), Some(vec![42]), false),
			Ok(WriteOutcome::Overwritten(3))
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([2; 32]), Some(vec![48]), true),
			Ok(WriteOutcome::Taken(vec![4, 5, 6]))
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([3; 32]), None, false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([4; 32]), None, true),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([5; 32]), Some(vec![]), false),
			Ok(WriteOutcome::Overwritten(0))
		);
		assert_eq!(
			ctx.ext.set_transient_storage(&Key::Fix([6; 32]), Some(vec![]), true),
			Ok(WriteOutcome::Taken(vec![]))
		);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter =
			TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>())
				.unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn get_transient_storage_works() {
	// Call stack: BOB -> CHARLIE(success) -> BOB' (success)
	let storage_key_1 = &Key::Fix([1; 32]);
	let storage_key_2 = &Key::Fix([2; 32]);
	let storage_key_3 = &Key::Fix([3; 32]);
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		if ctx.input_data[0] == 0 {
			assert_eq!(
				ctx.ext.set_transient_storage(storage_key_1, Some(vec![1, 2]), false),
				Ok(WriteOutcome::New)
			);
			assert_eq!(
				ctx.ext
					.call(
						&Default::default(),
						&CHARLIE_ADDR,
						U256::zero(),
						vec![],
						ReentrancyProtection::AllowReentry,
						false,
					)
					.map(|_| ctx.ext.last_frame_output().clone()),
				exec_success()
			);
			assert_eq!(ctx.ext.get_transient_storage(storage_key_1), Some(vec![3]));
			assert_eq!(ctx.ext.get_transient_storage(storage_key_2), Some(vec![]));
			assert_eq!(ctx.ext.get_transient_storage(storage_key_3), None);
		} else {
			assert_eq!(
				ctx.ext.set_transient_storage(storage_key_1, Some(vec![3]), true),
				Ok(WriteOutcome::Taken(vec![1, 2]))
			);
			assert_eq!(
				ctx.ext.set_transient_storage(storage_key_2, Some(vec![]), false),
				Ok(WriteOutcome::New)
			);
		}
		exec_success()
	});
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		assert!(
			ctx.ext
				.call(
					&Default::default(),
					&BOB_ADDR,
					U256::zero(),
					vec![99],
					ReentrancyProtection::AllowReentry,
					false
				)
				.is_ok()
		);
		// CHARLIE can not read BOB`s storage.
		assert_eq!(ctx.ext.get_transient_storage(storage_key_1), None);
		exec_success()
	});

	// This one tests passing the input data into a contract via call.
	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn get_transient_storage_size_works() {
	let storage_key_1 = &Key::Fix([1; 32]);
	let storage_key_2 = &Key::Fix([2; 32]);
	let storage_key_3 = &Key::Fix([3; 32]);
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_eq!(
			ctx.ext.set_transient_storage(storage_key_1, Some(vec![1, 2, 3]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(
			ctx.ext.set_transient_storage(storage_key_2, Some(vec![]), false),
			Ok(WriteOutcome::New)
		);
		assert_eq!(ctx.ext.get_transient_storage_size(storage_key_1), Some(3));
		assert_eq!(ctx.ext.get_transient_storage_size(storage_key_2), Some(0));
		assert_eq!(ctx.ext.get_transient_storage_size(storage_key_3), None);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_hash);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		assert_ok!(MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		));
	});
}

#[test]
fn rollback_transient_storage_works() {
	// Call stack: BOB -> CHARLIE (trap) -> BOB' (success)
	let storage_key = &Key::Fix([1; 32]);
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		if ctx.input_data[0] == 0 {
			assert_eq!(
				ctx.ext.set_transient_storage(storage_key, Some(vec![1, 2]), false),
				Ok(WriteOutcome::New)
			);
			assert_eq!(
				ctx.ext
					.call(
						&Default::default(),
						&CHARLIE_ADDR,
						U256::zero(),
						vec![],
						ReentrancyProtection::AllowReentry,
						false
					)
					.map(|_| ctx.ext.last_frame_output().clone()),
				exec_trapped()
			);
			assert_eq!(ctx.ext.get_transient_storage(storage_key), Some(vec![1, 2]));
		} else {
			let overwritten_length = ctx.ext.get_transient_storage_size(storage_key).unwrap();
			assert_eq!(
				ctx.ext.set_transient_storage(storage_key, Some(vec![3]), false),
				Ok(WriteOutcome::Overwritten(overwritten_length))
			);
			assert_eq!(ctx.ext.get_transient_storage(storage_key), Some(vec![3]));
		}
		exec_success()
	});
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		assert!(
			ctx.ext
				.call(
					&Default::default(),
					&BOB_ADDR,
					U256::zero(),
					vec![99],
					ReentrancyProtection::AllowReentry,
					false
				)
				.is_ok()
		);
		exec_trapped()
	});

	// This one tests passing the input data into a contract via call.
	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn ecdsa_to_eth_address_returns_proper_value() {
	let bob_ch = MockLoader::insert(Call, |ctx, _| {
		let pubkey_compressed = array_bytes::hex2array_unchecked(
			"028db55b05db86c0b1786ca49f095d76344c9e6056b2f02701a7e7f3c20aabfd91",
		);
		assert_eq!(
			ctx.ext.ecdsa_to_eth_address(&pubkey_compressed).unwrap(),
			array_bytes::hex2array_unchecked::<_, 20>("09231da7b19A016f9e576d23B16277062F4d46A8")
		);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, bob_ch);

		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn last_frame_output_works_on_instantiate() {
	let ok_ch = MockLoader::insert(Constructor, move |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: vec![127] })
	});
	let revert_ch = MockLoader::insert(Constructor, move |_, _| {
		Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![70] })
	});
	let trap_ch = MockLoader::insert(Constructor, |_, _| Err("It's a trap!".into()));
	let instantiator_ch = MockLoader::insert(Call, {
		move |ctx, _| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let value = Pallet::<Test>::convert_native_to_evm(min_balance);

			// Successful instantiation should set the output
			let address = ctx
				.ext
				.instantiate(&CallResources::NoLimits, Code::Existing(ok_ch), value, vec![], None)
				.unwrap();
			assert_eq!(
				ctx.ext.last_frame_output(),
				&ExecReturnValue { flags: ReturnFlags::empty(), data: vec![127] }
			);

			// Balance transfers should reset the output
			ctx.ext
				.call(
					&CallResources::from_weight_and_deposit(Weight::MAX, U256::MAX),
					&address,
					Pallet::<Test>::convert_native_to_evm(1),
					vec![],
					ReentrancyProtection::AllowReentry,
					false,
				)
				.unwrap();
			assert_eq!(ctx.ext.last_frame_output(), &Default::default());

			// Reverted instantiation should set the output
			ctx.ext
				.instantiate(&Default::default(), Code::Existing(revert_ch), value, vec![], None)
				.unwrap();
			assert_eq!(
				ctx.ext.last_frame_output(),
				&ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![70] }
			);

			// Trapped instantiation should clear the output
			ctx.ext
				.instantiate(&Default::default(), Code::Existing(trap_ch), value, vec![], None)
				.unwrap_err();
			assert_eq!(
				ctx.ext.last_frame_output(),
				&ExecReturnValue { flags: ReturnFlags::empty(), data: vec![] }
			);

			exec_success()
		}
	});

	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			set_balance(&ALICE, 1000);
			set_balance(&BOB, 100);
			place_contract(&BOB, instantiator_ch);
			let origin = Origin::from_account_id(ALICE);
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 200).unwrap();

			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap()
		});
}

#[test]
fn last_frame_output_works_on_nested_call() {
	// Call stack: BOB -> CHARLIE(revert) -> BOB' (success)
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		if ctx.input_data.is_empty() {
			// We didn't do anything yet
			assert_eq!(
				ctx.ext.last_frame_output(),
				&ExecReturnValue { flags: ReturnFlags::empty(), data: vec![] }
			);

			ctx.ext
				.call(
					&Default::default(),
					&CHARLIE_ADDR,
					U256::zero(),
					vec![],
					ReentrancyProtection::AllowReentry,
					false,
				)
				.unwrap();
			assert_eq!(
				ctx.ext.last_frame_output(),
				&ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![70] }
			);
		}

		Ok(ExecReturnValue { flags: ReturnFlags::empty(), data: vec![127] })
	});
	let code_charlie = MockLoader::insert(Call, |ctx, _| {
		// We didn't do anything yet
		assert_eq!(
			ctx.ext.last_frame_output(),
			&ExecReturnValue { flags: ReturnFlags::empty(), data: vec![] }
		);

		assert!(
			ctx.ext
				.call(
					&Default::default(),
					&BOB_ADDR,
					U256::zero(),
					vec![99],
					ReentrancyProtection::AllowReentry,
					false
				)
				.is_ok()
		);
		assert_eq!(
			ctx.ext.last_frame_output(),
			&ExecReturnValue { flags: ReturnFlags::empty(), data: vec![127] }
		);

		Ok(ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![70] })
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		place_contract(&CHARLIE, code_charlie);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![0],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn last_frame_output_is_always_reset() {
	let code_bob = MockLoader::insert(Call, |ctx, _| {
		let invalid_code_hash = H256::from_low_u64_le(u64::MAX);
		let output_revert = || ExecReturnValue { flags: ReturnFlags::REVERT, data: vec![1] };

		// A value of u256::MAX to fail the call on the first condition.
		*ctx.ext.last_frame_output_mut() = output_revert();
		assert_eq!(
			ctx.ext.call(
				&Default::default(),
				&H160::zero(),
				U256::max_value(),
				vec![],
				ReentrancyProtection::AllowReentry,
				false,
			),
			Err(Error::<Test>::BalanceConversionFailed.into())
		);
		assert_eq!(ctx.ext.last_frame_output(), &Default::default());

		// An unknown code hash should succeed but clear the output.
		*ctx.ext.last_frame_output_mut() = output_revert();
		assert_ok!(ctx.ext.delegate_call(
			&Default::default(),
			H160([0xff; 20]),
			Default::default()
		));
		assert_eq!(ctx.ext.last_frame_output(), &Default::default());

		// An unknown code hash to fail instantiation on the first condition.
		*ctx.ext.last_frame_output_mut() = output_revert();
		assert_eq!(
			ctx.ext.instantiate(
				&Default::default(),
				Code::Existing(invalid_code_hash),
				U256::zero(),
				vec![],
				None,
			),
			Err(Error::<Test>::CodeNotFound.into())
		);
		assert_eq!(ctx.ext.last_frame_output(), &Default::default());

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_bob);
		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let result = MockStack::run_call(
			origin,
			BOB_ADDR,
			&mut meter,
			U256::zero(),
			vec![],
			&ExecConfig::new_substrate_tx(),
		);
		assert_matches!(result, Ok(_));
	});
}

#[test]
fn immutable_data_access_checks_work() {
	let dummy_ch = MockLoader::insert(Constructor, move |ctx, _| {
		// Calls can not store immutable data
		assert_eq!(ctx.ext.get_immutable_data(), Err(Error::<Test>::InvalidImmutableAccess.into()));
		exec_success()
	});
	let instantiator_ch = MockLoader::insert(Call, {
		move |ctx, _| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let value = Pallet::<Test>::convert_native_to_evm(min_balance);

			assert_eq!(
				ctx.ext.set_immutable_data(vec![0, 1, 2, 3].try_into().unwrap()),
				Err(Error::<Test>::InvalidImmutableAccess.into())
			);

			// Constructors can not access the immutable data
			ctx.ext
				.instantiate(
					&CallResources::NoLimits,
					Code::Existing(dummy_ch),
					value,
					vec![],
					None,
				)
				.unwrap();

			exec_success()
		}
	});
	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			set_balance(&ALICE, 1000);
			set_balance(&BOB, 100);
			place_contract(&BOB, instantiator_ch);
			let origin = Origin::from_account_id(ALICE);
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 200).unwrap();

			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap()
		});
}

#[test]
fn correct_immutable_data_in_delegate_call() {
	let charlie_ch = MockLoader::insert(Call, |ctx, _| {
		Ok(ExecReturnValue {
			flags: ReturnFlags::empty(),
			data: ctx.ext.get_immutable_data()?.to_vec(),
		})
	});
	let bob_ch = MockLoader::insert(Call, move |ctx, _| {
		// In a regular call, we should witness the callee immutable data
		assert_eq!(
			ctx.ext
				.call(
					&Default::default(),
					&CHARLIE_ADDR,
					U256::zero(),
					vec![],
					ReentrancyProtection::AllowReentry,
					false,
				)
				.map(|_| ctx.ext.last_frame_output().data.clone()),
			Ok(vec![2]),
		);

		// Also in a delegate call, we should witness the callee immutable data
		assert_eq!(
			ctx.ext.delegate_call(&Default::default(), CHARLIE_ADDR, Vec::new()).map(|_| ctx
				.ext
				.last_frame_output()
				.data
				.clone()),
			Ok(vec![2])
		);

		exec_success()
	});
	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			place_contract(&BOB, bob_ch);
			place_contract(&CHARLIE, charlie_ch);

			let origin = Origin::from_account_id(ALICE);
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 200).unwrap();

			// Place unique immutable data for each contract
			<ImmutableDataOf<Test>>::insert::<_, ImmutableData>(
				BOB_ADDR,
				vec![1].try_into().unwrap(),
			);
			<ImmutableDataOf<Test>>::insert::<_, ImmutableData>(
				CHARLIE_ADDR,
				vec![2].try_into().unwrap(),
			);

			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap()
		});
}

#[test]
fn immutable_data_set_overrides() {
	let hash = MockLoader::insert_both(
		move |ctx, _| {
			// Calling `set_immutable_data` the first time should work
			assert_ok!(ctx.ext.set_immutable_data(vec![0, 1, 2, 3].try_into().unwrap()));
			// Calling `set_immutable_data` the second time overrides the original one
			assert_ok!(ctx.ext.set_immutable_data(vec![7, 5].try_into().unwrap()));
			exec_success()
		},
		move |ctx, _| {
			assert_eq!(ctx.ext.get_immutable_data().unwrap().into_inner(), vec![7, 5]);
			exec_success()
		},
	);
	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			set_balance(&ALICE, 1000);
			let origin = Origin::from_account_id(ALICE);
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 200).unwrap();

			let addr = MockStack::run_instantiate(
				ALICE,
				MockExecutable::from_storage(hash, &mut meter).unwrap(),
				&mut meter,
				U256::zero(),
				vec![],
				None,
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap()
			.0;

			MockStack::run_call(
				origin,
				addr,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap()
		});
}

#[test]
fn immutable_data_set_errors_with_empty_data() {
	let dummy_ch = MockLoader::insert(Constructor, move |ctx, _| {
		// Calling `set_immutable_data` with empty data should error out
		assert_eq!(
			ctx.ext.set_immutable_data(Default::default()),
			Err(Error::<Test>::InvalidImmutableAccess.into())
		);
		exec_success()
	});
	let instantiator_ch = MockLoader::insert(Call, {
		move |ctx, _| {
			let min_balance = <Test as Config>::Currency::minimum_balance();
			let value = Pallet::<Test>::convert_native_to_evm(min_balance);

			ctx.ext
				.instantiate(
					&CallResources::NoLimits,
					Code::Existing(dummy_ch),
					value,
					vec![],
					None,
				)
				.unwrap();

			exec_success()
		}
	});
	ExtBuilder::default()
		.with_code_hashes(MockLoader::code_hashes())
		.existential_deposit(15)
		.build()
		.execute_with(|| {
			set_balance(&ALICE, 1000);
			set_balance(&BOB, 100);
			place_contract(&BOB, instantiator_ch);
			let origin = Origin::from_account_id(ALICE);
			let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 200).unwrap();

			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
			.unwrap()
		});
}

#[test]
fn block_hash_returns_proper_values() {
	let bob_code_hash = MockLoader::insert(Call, |ctx, _| {
		ctx.ext.block_number = 1u32.into();
		assert_eq!(ctx.ext.block_hash(U256::from(1)), None);
		assert!(ctx.ext.block_hash(U256::from(0)).is_some());

		ctx.ext.block_number = 300u32.into();
		assert_eq!(ctx.ext.block_hash(U256::from(300)), None);
		assert_eq!(ctx.ext.block_hash(U256::from(43)), None);
		assert_eq!(ctx.ext.block_hash(U256::from(44)), Some(H256::from([2; 32])));

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		frame_system::BlockHash::<Test>::insert(
			&BlockNumberFor::<Test>::from(0u32),
			<tests::Test as frame_system::Config>::Hash::from([1; 32]),
		);
		frame_system::BlockHash::<Test>::insert(
			&BlockNumberFor::<Test>::from(1u32),
			<tests::Test as frame_system::Config>::Hash::default(),
		);
		frame_system::BlockHash::<Test>::insert(
			&BlockNumberFor::<Test>::from(43u32),
			<tests::Test as frame_system::Config>::Hash::default(),
		);
		frame_system::BlockHash::<Test>::insert(
			&BlockNumberFor::<Test>::from(44u32),
			<tests::Test as frame_system::Config>::Hash::from([2; 32]),
		);
		frame_system::BlockHash::<Test>::insert(
			&BlockNumberFor::<Test>::from(300u32),
			<tests::Test as frame_system::Config>::Hash::default(),
		);

		place_contract(&BOB, bob_code_hash);

		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();
		assert_matches!(
			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![0],
				&ExecConfig::new_substrate_tx(),
			),
			Ok(_)
		);
	});
}

#[test]
fn delegatecall_tracer_reports_correct_addresses() {
	// This test verifies that the callTracer correctly reports addresses for delegatecall.
	// Before the fix, for a delegatecall, the tracer incorrectly reported:
	//   from = original caller (ALICE), to = delegating contract (BOB)
	// After the fix, it correctly reports:
	//   from = delegating contract (BOB), to = delegate target (CHARLIE)
	use crate::{
		evm::{CallTracer, CallType},
		tracing::trace,
	};

	let target_ch = MockLoader::insert(Call, |_, _| exec_success());

	let delegate_ch = MockLoader::insert(Call, move |ctx, _| {
		ctx.ext.delegate_call(&Default::default(), CHARLIE_ADDR, Vec::new())?;
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, delegate_ch);
		place_contract(&CHARLIE, target_ch);
		set_balance(&ALICE, 100);

		let origin = Origin::from_account_id(ALICE);
		let mut meter = TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, 0).unwrap();

		let mut tracer = CallTracer::new(Default::default());
		let result = trace(&mut tracer, || {
			MockStack::run_call(
				origin,
				BOB_ADDR,
				&mut meter,
				U256::zero(),
				vec![],
				&ExecConfig::new_substrate_tx(),
			)
		});
		assert_ok!(result);

		let call_trace = tracer.collect_trace().unwrap();

		// Top-level call: ALICE calls BOB
		assert_eq!(call_trace.from, ALICE_ADDR);
		assert_eq!(call_trace.to, BOB_ADDR);
		assert_eq!(call_trace.call_type, CallType::Call);

		// Nested delegatecall trace: BOB delegate-calls CHARLIE
		assert_eq!(call_trace.calls.len(), 1);
		let delegate_trace = &call_trace.calls[0];
		assert_eq!(delegate_trace.call_type, CallType::DelegateCall);

		// This is the critical assertion for the fix:
		// Pre-fix (WRONG): from = ALICE_ADDR, to = BOB_ADDR
		// Post-fix (CORRECT): from = BOB_ADDR, to = CHARLIE_ADDR
		assert_eq!(
			delegate_trace.from, BOB_ADDR,
			"delegatecall 'from' should be the contract making the delegatecall (BOB)"
		);
		assert_eq!(
			delegate_trace.to, CHARLIE_ADDR,
			"delegatecall 'to' should be the delegate target (CHARLIE)"
		);
	});
}

fn is_cold_touch<E: Ext>(ext: &mut E, key: &Key) -> bool {
	matches!(
		ext.touch_storage_access(false, key, StorageOp::Read),
		StorageAccessKind::Persistent(Warmth::Cold { .. })
	)
}

fn run_root_call(contract_addr: H160, input: Vec<u8>) {
	set_balance(&ALICE, <Test as Config>::Currency::minimum_balance() * 1000);
	let mut meter =
		TransactionMeter::<Test>::new_from_limits(WEIGHT_LIMIT, deposit_limit::<Test>()).unwrap();
	assert_ok!(MockStack::run_call(
		Origin::from_account_id(ALICE),
		contract_addr,
		&mut meter,
		0u32.into(),
		input,
		&ExecConfig::new_substrate_tx(),
	));
}

fn run_child_call<E: Ext>(ext: &mut E, to: &H160, input: Vec<u8>) -> Result<(), ExecError> {
	ext.call(
		&CallResources::NoLimits,
		to,
		U256::zero(),
		input,
		ReentrancyProtection::AllowReentry,
		false,
	)
}

#[test]
fn cold_hot_retouch_marks_slot_hot() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		let key_a = Key::Fix([7; 32]);
		let key_b = Key::Fix([8; 32]);

		assert!(is_cold_touch(ctx.ext, &key_a), "first touch on key_a is cold");
		assert!(!is_cold_touch(ctx.ext, &key_a), "second touch on key_a is hot");
		assert!(is_cold_touch(ctx.ext, &key_b), "first touch on a distinct slot is cold");

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_hash);
		run_root_call(BOB_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_child_commit_visible_on_next_call() {
	// A committed child touch on (addr, slot) stays hot on the next child call —
	// and parent's own touch on `slot` is a separate entry (address discriminates).
	const SLOT: [u8; 32] = [9; 32];

	let child_code_hash = MockLoader::insert(Call, |ctx, _| {
		let expected = ctx.input_data[0] != 0;
		let actual = is_cold_touch(ctx.ext, &Key::Fix(SLOT));
		assert_eq!(actual, expected, "child expected cold={expected}");
		exec_success()
	});

	let parent_code_hash = MockLoader::insert(Call, |ctx, _| {
		// First child call: cold, commits.
		assert_matches!(run_child_call(ctx.ext, &BOB_ADDR, vec![1]), Ok(_));

		// Parent's own touch is a different access-list entry, still cold.
		assert!(is_cold_touch(ctx.ext, &Key::Fix(SLOT)));

		// Second child call: now hot — first commit is still visible.
		assert_matches!(run_child_call(ctx.ext, &BOB_ADDR, vec![0]), Ok(_));
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, child_code_hash);
		place_contract(&CHARLIE, parent_code_hash);
		run_root_call(CHARLIE_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_var_inline_len_distinguishes() {
	// After zero-padding to `[u8; 36]`, both keys share the same `bytes`
	// field; only `len` distinguishes them.
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		let short_key = Key::try_from_var(vec![1, 2, 3]).unwrap();
		let long_key = Key::try_from_var(vec![1, 2, 3, 0]).unwrap();

		assert!(is_cold_touch(ctx.ext, &short_key));
		assert!(
			is_cold_touch(ctx.ext, &long_key),
			"VarInline keys with same byte prefix but different `len` must NOT alias",
		);

		let inline_max = Key::try_from_var(vec![7u8; MAX_INLINE_KEY_LEN]).unwrap();
		let just_over = Key::try_from_var(vec![7u8; MAX_INLINE_KEY_LEN + 1]).unwrap();

		assert!(is_cold_touch(ctx.ext, &inline_max), "{MAX_INLINE_KEY_LEN}-byte key is cold");
		assert!(
			is_cold_touch(ctx.ext, &just_over),
			"{}-byte key crosses into VarLong and must NOT alias with the VarInline at the cap",
			MAX_INLINE_KEY_LEN + 1,
		);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_hash);
		run_root_call(BOB_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_delegate_call_marks_parent_slot_hot() {
	// Delegate-call runs the callee's code in the caller's storage context, so
	// a touch inside the child keys on the parent's address — the same entry
	// the parent will see when it touches the slot itself.
	const SLOT: [u8; 32] = [55; 32];

	let child_code_hash = MockLoader::insert(Call, |ctx, _| {
		assert!(is_cold_touch(ctx.ext, &Key::Fix(SLOT)), "delegate-callee's first touch is cold",);
		exec_success()
	});

	let parent_code_hash = MockLoader::insert(Call, |ctx, _| {
		ctx.ext
			.delegate_call(&Default::default(), BOB_ADDR, vec![])
			.expect("delegate-call to child must succeed");
		assert!(
			!is_cold_touch(ctx.ext, &Key::Fix(SLOT)),
			"parent's touch is hot — delegate-call warmed it",
		);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, child_code_hash);
		place_contract(&CHARLIE, parent_code_hash);
		run_root_call(CHARLIE_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_revertible_only_inside_nested_frame() {
	const SLOT: [u8; 32] = [13; 32];

	let child_code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_matches!(
			ctx.ext.touch_storage_access(false, &Key::Fix(SLOT), StorageOp::Read),
			StorageAccessKind::Persistent(Warmth::Cold { revertible: true }),
			"a cold touch in a nested frame is revertible",
		);
		exec_success()
	});

	let root_code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_matches!(
			ctx.ext.touch_storage_access(false, &Key::Fix(SLOT), StorageOp::Read),
			StorageAccessKind::Persistent(Warmth::Cold { revertible: false }),
			"a cold touch in the root frame is not revertible",
		);
		assert_matches!(run_child_call(ctx.ext, &BOB_ADDR, vec![]), Ok(_));
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, child_code_hash);
		place_contract(&CHARLIE, root_code_hash);
		run_root_call(CHARLIE_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_past_cap_touch_is_not_revertible() {
	let child_code_hash = MockLoader::insert(Call, |ctx, _| {
		// Fill the access list to its cap with distinct slots. Each is below the
		// cap when touched, so it journals and is revertible.
		for i in 0..MAX_ACCESS_LIST_ENTRIES as u32 {
			let mut slot = [0u8; 32];
			slot[..4].copy_from_slice(&i.to_le_bytes());
			assert_matches!(
				ctx.ext.touch_storage_access(false, &Key::Fix(slot), StorageOp::Read),
				StorageAccessKind::Persistent(Warmth::Cold { revertible: true })
			);
		}
		// A further distinct slot is past the cap: cold but not revertible.
		assert_matches!(
			ctx.ext.touch_storage_access(false, &Key::Fix([0xFF; 32]), StorageOp::Read),
			StorageAccessKind::Persistent(Warmth::Cold { revertible: false }),
			"past-cap touch is cold but not revertible",
		);
		exec_success()
	});

	let root_code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_matches!(run_child_call(ctx.ext, &BOB_ADDR, vec![]), Ok(_));
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, child_code_hash);
		place_contract(&CHARLIE, root_code_hash);
		run_root_call(CHARLIE_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_transient_skips_access_list() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		let key = Key::Fix([42; 32]);

		// `transient: true` classifies as `Transient` without touching the access list.
		let kind = ctx.ext.touch_storage_access(true, &key, StorageOp::Read);
		assert!(matches!(kind, StorageAccessKind::Transient));

		// The same key is still cold in the persistent access list.
		let persistent_kind = ctx.ext.peek_storage_access(false, &key);
		assert!(
			matches!(persistent_kind, StorageAccessKind::Persistent(Warmth::Cold { .. })),
			"transient access must not warm the persistent access list",
		);

		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_hash);
		run_root_call(BOB_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_child_upgrade_follows_the_frame_outcome() {
	let code_hash = MockLoader::insert(Call, |ctx, _| {
		let slot = Key::Fix([1; 32]);

		match ctx.input_data.as_slice() {
			[1] => {
				ctx.ext.touch_storage_access(false, &slot, StorageOp::Write);
				return Err("revert after upgrading".into());
			},
			[2] => {
				ctx.ext.touch_storage_access(false, &slot, StorageOp::Write);
				return exec_success();
			},
			_ => (),
		}

		ctx.ext.touch_storage_access(false, &slot, StorageOp::Read);

		assert_matches!(
			run_child_call(ctx.ext, &BOB_ADDR, vec![1]),
			Err(ExecError { error: DispatchError::Other("revert after upgrading"), .. }),
			"the child must fail at its own revert",
		);
		assert_matches!(
			ctx.ext.peek_storage_access(false, &slot),
			StorageAccessKind::Persistent(Warmth::Hot { charged: StorageOp::Read }),
			"the reverted child's upgrade must roll back, leaving the write unpaid",
		);

		assert!(run_child_call(ctx.ext, &BOB_ADDR, vec![2]).is_ok(), "the child must succeed");
		assert_matches!(
			ctx.ext.peek_storage_access(false, &slot),
			StorageAccessKind::Persistent(Warmth::Hot { charged: StorageOp::Write }),
			"the committed child's upgrade must stay",
		);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&BOB, code_hash);
		run_root_call(BOB_ADDR, vec![]);
	});
}

#[test]
fn cold_hot_3level_commit_then_revert_drops_committed() {
	// root → A → grandchild. Grandchild commits a touch into A; A then reverts,
	// dropping it. The next grandchild call must see the slot cold again.
	let grandchild_code_hash = MockLoader::insert(Call, |ctx, _| {
		let key = Key::Fix([99; 32]);
		assert!(
			is_cold_touch(ctx.ext, &key),
			"grandchild touch must be cold (A's revert should have dropped the committed entry)",
		);
		exec_success()
	});

	let a_code_hash = MockLoader::insert(Call, |ctx, _| {
		assert_matches!(run_child_call(ctx.ext, &DJANGO_ADDR, vec![]), Ok(_));
		Err("rollback A".into())
	});

	let root_code_hash = MockLoader::insert(Call, |ctx, _| {
		let _ = run_child_call(ctx.ext, &BOB_ADDR, vec![]);
		let _ = run_child_call(ctx.ext, &BOB_ADDR, vec![]);
		exec_success()
	});

	ExtBuilder::default().build().execute_with(|| {
		place_contract(&DJANGO, grandchild_code_hash);
		place_contract(&BOB, a_code_hash);
		place_contract(&CHARLIE, root_code_hash);
		run_root_call(CHARLIE_ADDR, vec![]);
	});
}
