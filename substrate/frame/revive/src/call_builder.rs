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

//! Types to build an environment that can be used to test and benchmark host function /
//! pre-compiles.

#![cfg(any(feature = "runtime-benchmarks", test))]
// A lot of this code is only used in benchmarking but we also want to use the code for testing
// pre-compiles eventually. For that we will probably export some of those types from the crate.
// Until then we simply ignore the warnings that arise when compiling tests without runtime
// benchmarks.
#![cfg_attr(test, allow(dead_code))]

use crate::{
	AccountInfo, BalanceOf, BalanceWithDust, Code, CodeInfoOf, Config, ContractBlob, ContractInfo,
	Error, ExecConfig, ExecOrigin as Origin, OriginFor, Pallet as Contracts, PristineCode, Weight,
	address::AddressMapper,
	exec::{ExportedFunction, Key, PrecompileExt, Stack},
	limits,
	metering::{TransactionLimits, TransactionMeter},
	transient_storage::MeterEntry,
	vm::pvm::{PreparedCall, Runtime},
};
use alloc::{vec, vec::Vec};
use frame_support::{storage::child, traits::fungible::Mutate};
use frame_system::RawOrigin;
use pallet_revive_fixtures::bench as bench_fixtures;
use sp_core::{H160, H256, U256};
use sp_io::hashing::keccak_256;
use sp_runtime::traits::{Bounded, Hash};

type StackExt<'a, T> = Stack<'a, T, ContractBlob<T>>;

/// A builder used to prepare a contract call.
pub struct CallSetup<T: Config> {
	contract: Contract<T>,
	dest: T::AccountId,
	origin: Origin<T>,
	transaction_meter: TransactionMeter<T>,
	value: BalanceOf<T>,
	data: Vec<u8>,
	transient_storage_size: u32,
	exec_config: ExecConfig<T>,
	read_only: bool,
	delegate_call: bool,
}

impl<T> Default for CallSetup<T>
where
	T: Config,
{
	fn default() -> Self {
		Self::new(VmBinaryModule::dummy())
	}
}

impl<T> CallSetup<T>
where
	T: Config,
{
	/// Setup a new call for the given module.
	pub fn new(module: VmBinaryModule) -> Self {
		let contract = Contract::<T>::new(module, vec![]).unwrap();
		let dest = contract.account_id.clone();
		let origin = Origin::from_account_id(contract.caller.clone());

		#[cfg(feature = "runtime-benchmarks")]
		{
			// Whitelist contract account, as it is already accounted for in the call benchmark
			frame_benchmarking::benchmarking::add_to_whitelist(
				frame_system::Account::<T>::hashed_key_for(&contract.account_id).into(),
			);

			// Whitelist the contract's contractInfo as it is already accounted for in the call
			// benchmark
			frame_benchmarking::benchmarking::add_to_whitelist(
				crate::AccountInfoOf::<T>::hashed_key_for(&T::AddressMapper::to_address(
					&contract.account_id,
				))
				.into(),
			);
		}

		Self {
			contract,
			dest,
			origin,
			transaction_meter: TransactionMeter::new(TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: default_deposit_limit::<T>(),
			})
			.unwrap(),
			value: 0u32.into(),
			data: vec![],
			transient_storage_size: 0,
			exec_config: ExecConfig::new_substrate_tx(),
			read_only: false,
			delegate_call: false,
		}
	}

	/// Set the meter's storage deposit limit.
	pub fn set_storage_deposit_limit(&mut self, balance: BalanceOf<T>) {
		self.transaction_meter = TransactionMeter::new(TransactionLimits::WeightAndDeposit {
			weight_limit: Weight::MAX,
			deposit_limit: balance,
		})
		.unwrap();
	}

	/// Set the call's origin.
	pub fn set_origin(&mut self, origin: Origin<T>) {
		self.origin = origin;
	}

	/// Set the contract's balance.
	pub fn set_balance(&mut self, value: impl Into<BalanceWithDust<BalanceOf<T>>>) {
		self.contract.set_balance(value);
	}

	/// Set the call's input data.
	pub fn set_data(&mut self, value: Vec<u8>) {
		self.data = value;
	}

	/// Set the transient storage size.
	pub fn set_transient_storage_size(&mut self, size: u32) {
		self.transient_storage_size = size;
	}

	/// Set the read-only flag on the call stack frame.
	pub fn set_read_only(&mut self, read_only: bool) {
		self.read_only = read_only;
	}

	/// Mark the call as a delegate call.
	pub fn set_delegate_call(&mut self, delegate: bool) {
		self.delegate_call = delegate;
	}

	/// Get the call's input data.
	pub fn data(&self) -> Vec<u8> {
		self.data.clone()
	}

	/// Get the call's contract.
	pub fn contract(&self) -> Contract<T> {
		self.contract.clone()
	}

	/// Build the call stack.
	pub fn ext(&mut self) -> (StackExt<'_, T>, ContractBlob<T>) {
		let mut ext = StackExt::bench_new_call(
			T::AddressMapper::to_address(&self.dest),
			self.origin.clone(),
			&mut self.transaction_meter,
			self.value,
			&self.exec_config,
			self.read_only,
			self.delegate_call,
		);
		if self.transient_storage_size > 0 {
			Self::with_transient_storage(&mut ext.0, self.transient_storage_size).unwrap();
		}
		ext
	}

	/// Prepare a call to the module.
	pub fn prepare_call<'a>(
		ext: &'a mut StackExt<'a, T>,
		module: ContractBlob<T>,
		input: Vec<u8>,
		aux_data_size: u32,
	) -> PreparedCall<'a, StackExt<'a, T>> {
		module
			.prepare_call(Runtime::new(ext, input), ExportedFunction::Call, aux_data_size)
			.unwrap()
	}

	/// Add transient_storage
	fn with_transient_storage(ext: &mut StackExt<T>, size: u32) -> Result<(), &'static str> {
		let &MeterEntry { amount, limit } = ext.transient_storage().meter().current();
		ext.transient_storage().meter().current_mut().limit = size;
		for i in 1u32.. {
			let mut key_data = i.to_le_bytes().to_vec();
			while key_data.last() == Some(&0) {
				key_data.pop();
			}
			let key = Key::try_from_var(key_data).unwrap();
			if let Err(e) = ext.set_transient_storage(&key, Some(Vec::new()), false) {
				// Restore previous settings.
				ext.transient_storage().meter().current_mut().limit = limit;
				ext.transient_storage().meter().current_mut().amount = amount;
				if e == Error::<T>::OutOfTransientStorage.into() {
					break;
				} else {
					return Err("Initialization of the transient storage failed");
				}
			}
		}
		Ok(())
	}
}

/// The deposit limit we use for benchmarks.
pub fn default_deposit_limit<T: Config>() -> BalanceOf<T> {
	<BalanceOf<T>>::max_value()
}

/// The funding that each account that either calls or instantiates contracts is funded with.
pub fn caller_funding<T: Config>() -> BalanceOf<T> {
	// Minting can overflow, so we can't abuse of the funding. This value happens to be big enough,
	// but not too big to make the total supply overflow.
	BalanceOf::<T>::max_value() / 10_000u32.into()
}

/// An instantiated and deployed contract.
#[derive(Clone)]
pub struct Contract<T: Config> {
	pub caller: T::AccountId,
	pub account_id: T::AccountId,
	pub address: H160,
}

impl<T> Contract<T>
where
	T: Config,
{
	/// Create new contract and use a default account id as instantiator.
	pub fn new(module: VmBinaryModule, data: Vec<u8>) -> Result<Contract<T>, &'static str> {
		let caller = T::AddressMapper::to_fallback_account_id(&crate::test_utils::ALICE_ADDR);
		Self::with_caller(caller, module, data)
	}

	/// Create new contract and use an account id derived from the supplied index as instantiator.
	#[cfg(feature = "runtime-benchmarks")]
	pub fn with_index(
		index: u32,
		module: VmBinaryModule,
		data: Vec<u8>,
	) -> Result<Contract<T>, &'static str> {
		Self::with_caller(frame_benchmarking::account("instantiator", index, 0), module, data)
	}

	/// Create new contract and use the supplied `caller` as instantiator.
	pub fn with_caller(
		caller: T::AccountId,
		module: VmBinaryModule,
		data: Vec<u8>,
	) -> Result<Contract<T>, &'static str> {
		T::Currency::set_balance(&caller, caller_funding::<T>());
		let salt = Some([0xffu8; 32]);
		let origin: OriginFor<T> = RawOrigin::Signed(caller.clone()).into();

		// We ignore the error since we might also pass an already mapped account here.
		Contracts::<T>::map_account(origin.clone()).ok();

		#[cfg(feature = "runtime-benchmarks")]
		frame_benchmarking::benchmarking::add_to_whitelist(
			frame_system::Account::<T>::hashed_key_for(&caller).into(),
		);

		let outcome = Contracts::<T>::bare_instantiate(
			origin,
			U256::zero(),
			TransactionLimits::WeightAndDeposit {
				weight_limit: Weight::MAX,
				deposit_limit: default_deposit_limit::<T>(),
			},
			Code::Upload(module.code),
			data,
			salt,
			&ExecConfig::new_substrate_tx(),
		);

		let address = outcome.result?.addr;
		let account_id = T::AddressMapper::to_fallback_account_id(&address);
		let result = Contract { caller, address, account_id };

		AccountInfo::<T>::insert_contract(&address, result.info()?);
		Ok(result)
	}

	/// Create a new contract with the supplied storage item count and size each.
	pub fn with_storage(
		code: VmBinaryModule,
		stor_num: u32,
		stor_size: u32,
	) -> Result<Self, &'static str> {
		let contract = Contract::<T>::new(code, vec![])?;
		let storage_items = (0..stor_num)
			.map(|i| {
				let hash = T::Hashing::hash_of(&i)
					.as_ref()
					.try_into()
					.map_err(|_| "Hash too big for storage key")?;
				Ok((hash, vec![42u8; stor_size as usize]))
			})
			.collect::<Result<Vec<_>, &'static str>>()?;
		contract.store(&storage_items)?;
		Ok(contract)
	}

	/// Store the supplied storage items into this contracts storage.
	pub fn store(&self, items: &Vec<([u8; 32], Vec<u8>)>) -> Result<(), &'static str> {
		let info = self.info()?;
		for item in items {
			info.write(&Key::Fix(item.0), Some(item.1.clone()), None, false)
				.map_err(|_| "Failed to write storage to restoration dest")?;
		}

		AccountInfo::<T>::insert_contract(&self.address, info);
		Ok(())
	}

	/// Create a new contract with the specified unbalanced storage trie.
	pub fn with_unbalanced_storage_trie(
		code: VmBinaryModule,
		key: &[u8],
	) -> Result<Self, &'static str> {
		/// Number of layers in a Radix16 unbalanced trie.
		const UNBALANCED_TRIE_LAYERS: u32 = 20;

		if (key.len() as u32) < UNBALANCED_TRIE_LAYERS.div_ceil(2) {
			return Err("Key size too small to create the specified trie");
		}

		let value = vec![16u8; limits::STORAGE_BYTES as usize];
		let contract = Contract::<T>::new(code, vec![])?;
		let info = contract.info()?;
		let child_trie_info = info.child_trie_info();
		child::put_raw(&child_trie_info, &key, &value);
		for l in 0..UNBALANCED_TRIE_LAYERS {
			let pos = l as usize / 2;
			let mut key_new = key.to_vec();
			for i in 0u8..16 {
				key_new[pos] = if l % 2 == 0 {
					(key_new[pos] & 0xF0) | i
				} else {
					(key_new[pos] & 0x0F) | (i << 4)
				};

				if key == &key_new {
					continue;
				}
				child::put_raw(&child_trie_info, &key_new, &value);
			}
		}
		Ok(contract)
	}

	/// Get the `ContractInfo` of the `addr` or an error if it no longer exists.
	pub fn address_info(addr: &T::AccountId) -> Result<ContractInfo<T>, &'static str> {
		<AccountInfo<T>>::load_contract(&T::AddressMapper::to_address(addr))
			.ok_or("Expected contract to exist at this point.")
	}

	/// Get the `ContractInfo` of this contract or an error if it no longer exists.
	pub fn info(&self) -> Result<ContractInfo<T>, &'static str> {
		Self::address_info(&self.account_id)
	}

	/// Set the balance of the contract to the supplied amount.
	pub fn set_balance(&self, value: impl Into<BalanceWithDust<BalanceOf<T>>>) {
		let (value, dust) = value.into().deconstruct();
		T::Currency::set_balance(&self.account_id, value);
		crate::AccountInfoOf::<T>::mutate(&self.address, |account| {
			account.as_mut().map(|a| a.dust = dust);
		});
	}

	/// Returns `true` iff all storage entries related to code storage exist.
	pub fn code_exists(hash: &sp_core::H256) -> bool {
		<PristineCode<T>>::contains_key(hash) && <CodeInfoOf<T>>::contains_key(&hash)
	}

	/// Returns `true` iff no storage entry related to code storage exist.
	pub fn code_removed(hash: &sp_core::H256) -> bool {
		!<PristineCode<T>>::contains_key(hash) && !<CodeInfoOf<T>>::contains_key(&hash)
	}
}

/// A vm binary module ready to be put on chain.
#[derive(Clone)]
pub struct VmBinaryModule {
	pub code: Vec<u8>,
	pub hash: H256,
}

impl VmBinaryModule {
	/// Return a contract code that does nothing.
	pub fn dummy() -> Self {
		Self::new(bench_fixtures::dummy().to_vec())
	}

	fn new(code: Vec<u8>) -> Self {
		let hash = keccak_256(&code);
		Self { code, hash: H256(hash) }
	}
}

#[cfg(any(test, feature = "runtime-benchmarks"))]
impl VmBinaryModule {
	// Creates EVM init code that deploys `size` bytes of runtime code (all STOP opcodes).
	// EVM memory is zero-initialized, so RETURN(0, size) produces `size` bytes of 0x00.
	// The runtime code is what gets stored in PristineCode and loaded on every call.
	pub fn evm_init_code_for_runtime_size(size: u32) -> Self {
		use revm::bytecode::opcode::{PUSH1, PUSH3, RETURN};
		assert!(size <= 0x00FF_FFFF, "size {size} exceeds PUSH3 max (16MiB - 1)");
		let [_, b1, b2, b3] = size.to_be_bytes();
		let code = vec![
			PUSH3, b1, b2, b3, // push runtime code size
			PUSH1, 0,      // push memory offset 0
			RETURN, // return `size` bytes from memory as runtime code
		];
		Self::new(code)
	}
}

#[cfg(feature = "runtime-benchmarks")]
impl VmBinaryModule {
	/// Same as [`Self::dummy`] but uses `replace_with` to make the code unique.
	pub fn dummy_unique(replace_with: u32) -> Self {
		Self::new(bench_fixtures::dummy_unique(replace_with))
	}

	/// Same as as `with_num_instructions` but based on the blob size.
	///
	/// This is needed when we weigh a blob without knowing how much instructions it
	/// contains.
	pub fn sized(size: u32) -> Self {
		// Due to variable length encoding of instructions this is not precise. But we only
		// need rough numbers for our benchmarks.
		Self::with_num_instructions(size / 3)
	}

	/// A contract code of specified number of instructions that uses all its bytes for instructions
	/// but will return immediately.
	///
	/// All the basic blocks are maximum sized (only the first is important though). This is to
	/// account for the fact that the interpreter will compile one basic block at a time even
	/// when no code is executed. Hence this contract will trigger the compilation of a maximum
	/// sized basic block and then return with its first instruction.
	///
	/// All the code will be put into the "call" export. Hence this code can be safely used for the
	/// `instantiate_with_code` benchmark where no compilation of any block should be measured.
	pub fn with_num_instructions(num_instructions: u32) -> Self {
		use alloc::{fmt::Write, string::ToString};
		let mut text = "
		pub @deploy:
		ret
		pub @call:
		"
		.to_string();
		for i in 0..num_instructions {
			match i {
				// return execution right away without breaking up basic block
				// SENTINEL is a hard coded syscall that terminates execution
				0 => writeln!(text, "ecalli {}", crate::SENTINEL).unwrap(),
				i if i % (limits::code::BASIC_BLOCK_SIZE - 1) == 0 => {
					text.push_str("fallthrough\n")
				},
				_ => text.push_str("a0 = a1 + a2\n"),
			}
		}
		text.push_str("ret\n");
		let code = polkavm_common::assembler::assemble(
			Some(polkavm_common::program::InstructionSetKind::ReviveV1),
			&text,
		)
		.unwrap();
		Self::new(code)
	}

	/// A contract code that calls the "noop" host function in a loop depending in the input.
	pub fn noop() -> Self {
		Self::new(bench_fixtures::noop().to_vec())
	}

	/// A contract code that does unaligned memory accessed in a loop.
	pub fn instr(do_load: bool) -> Self {
		let load = match do_load {
			false => "",
			true => "a0 = u64 [a0]",
		};
		let text = alloc::format!(
			"
		pub @deploy:
		ret
		pub @call:
			@loop:
				jump @done if t0 == a1
				{load}
				t0 = t0 + 1
				jump @loop
			@done:
		ret
		"
		);
		let code = polkavm_common::assembler::assemble(
			Some(polkavm_common::program::InstructionSetKind::ReviveV1),
			&text,
		)
		.unwrap();
		Self::new(code)
	}

	/// An evm contract that executes `size` JUMPDEST instructions.
	pub fn evm_noop(size: u32) -> Self {
		use revm::bytecode::opcode::JUMPDEST;

		let code = vec![JUMPDEST; size as usize];
		Self::new(code)
	}
}
