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
use crate::{
	AccountIdOf, BalanceOf, CodeInfo, Config, ContractBlob, DispatchError, Error, H256, LOG_TARGET,
	Weight,
	debug::DebugSettings,
	precompiles::Token,
	tracing,
	vm::{BytecodeType, ExecResult, Ext, evm::instructions::exec_instruction},
	weights::WeightInfo,
};
use alloc::vec::Vec;
use core::{convert::Infallible, ops::ControlFlow};
use revm::{bytecode::Bytecode, primitives::Bytes};

#[cfg(feature = "runtime-benchmarks")]
pub mod instructions;
#[cfg(not(feature = "runtime-benchmarks"))]
mod instructions;

mod interpreter;
pub use interpreter::{Halt, Interpreter};

mod ext_bytecode;
pub use ext_bytecode::ExtBytecode;

mod memory;
mod stack;
mod util;

/// Hard-coded value returned by the EVM `DIFFICULTY` opcode.
///
/// After Ethereum's Merge (Sept 2022), the `DIFFICULTY` opcode was redefined to return
/// `prevrandao`, a randomness value from the beacon chain. In Substrate pallet-revive
/// a fixed constant is returned instead for compatibility with contracts that still read this
/// opcode. The value is aligned with the difficulty hardcoded for PVM contracts.
pub(crate) const DIFFICULTY: u64 = 2500000000000000_u64;

/// Cost  for a single unit of EVM gas.
#[derive(Eq, PartialEq, Debug, Clone, Copy)]
pub struct EVMGas(pub u64);

impl<T: Config> Token<T> for EVMGas {
	fn weight(&self) -> Weight {
		let base_cost = T::WeightInfo::evm_opcode(1).saturating_sub(T::WeightInfo::evm_opcode(0));
		base_cost.saturating_mul(self.0)
	}
}

/// The cost of an EVM op-code that is charged from its own benchmark rather than in [`EVMGas`].
#[derive(Eq, PartialEq, Debug, Clone, Copy)]
enum EvmOpcodeCosts {
	Div,
	SDiv,
	Mod,
	SMod,
	AddMod,
	MulMod,
}

impl<T: Config> Token<T> for EvmOpcodeCosts {
	fn weight(&self) -> Weight {
		let per_opcode = |weight_fn: fn(u32) -> Weight| weight_fn(1).saturating_sub(weight_fn(0));
		// These op-codes are benchmarked in pairs with a `POP`, whose cost isn't theirs.
		let pop = per_opcode(T::WeightInfo::evm_pop_opcode);
		match self {
			Self::Div => per_opcode(T::WeightInfo::evm_div_opcode)
				.max(per_opcode(T::WeightInfo::evm_div_opcode_one_limb_variant))
				.max(per_opcode(T::WeightInfo::evm_div_opcode_mixed_variant))
				.max(per_opcode(T::WeightInfo::evm_div_opcode_fixed_knuth_variant))
				.saturating_sub(pop),
			Self::SDiv => per_opcode(T::WeightInfo::evm_sdiv_opcode)
				.max(per_opcode(T::WeightInfo::evm_sdiv_opcode_one_limb_variant))
				.max(per_opcode(T::WeightInfo::evm_sdiv_opcode_mixed_variant))
				.max(per_opcode(T::WeightInfo::evm_sdiv_opcode_fixed_knuth_variant))
				.saturating_sub(pop),
			Self::Mod => per_opcode(T::WeightInfo::evm_mod_opcode)
				.max(per_opcode(T::WeightInfo::evm_mod_opcode_one_limb_variant))
				.max(per_opcode(T::WeightInfo::evm_mod_opcode_mixed_variant))
				.max(per_opcode(T::WeightInfo::evm_mod_opcode_fixed_knuth_variant))
				.saturating_sub(pop),
			Self::SMod => per_opcode(T::WeightInfo::evm_smod_opcode)
				.max(per_opcode(T::WeightInfo::evm_smod_opcode_one_limb_variant))
				.max(per_opcode(T::WeightInfo::evm_smod_opcode_mixed_variant))
				.max(per_opcode(T::WeightInfo::evm_smod_opcode_fixed_knuth_variant))
				.saturating_sub(pop),
			Self::AddMod => per_opcode(T::WeightInfo::evm_addmod_opcode)
				.max(per_opcode(T::WeightInfo::evm_addmod_opcode_one_limb_variant))
				.max(per_opcode(T::WeightInfo::evm_addmod_opcode_mixed_variant))
				.max(per_opcode(T::WeightInfo::evm_addmod_opcode_fixed_knuth_variant))
				.saturating_sub(pop),
			Self::MulMod => per_opcode(T::WeightInfo::evm_mulmod_opcode).saturating_sub(pop),
		}
	}
}

impl<T: Config> ContractBlob<T> {
	/// Create a new contract from EVM init code.
	pub fn from_evm_init_code(code: Vec<u8>, owner: AccountIdOf<T>) -> Result<Self, DispatchError> {
		if code.len() > revm::primitives::eip3860::MAX_INITCODE_SIZE &&
			!DebugSettings::is_unlimited_contract_size_allowed::<T>()
		{
			return Err(<Error<T>>::BlobTooLarge.into());
		}

		// EIP-3541: Reject new contract code starting with the 0xEF byte
		if code.first() == Some(&0xEF) {
			return Err(<Error<T>>::CodeRejected.into());
		}

		let code_len = code.len() as u32;
		let code_info = CodeInfo {
			owner,
			deposit: Default::default(),
			refcount: 0,
			code_len,
			code_type: BytecodeType::Evm,
			behaviour_version: Default::default(),
		};

		Bytecode::new_raw_checked(Bytes::from(code.to_vec())).map_err(|err| {
			log::debug!(target: LOG_TARGET, "failed to create evm bytecode from init code: {err:?}" );
			<Error<T>>::CodeRejected
		})?;

		// Code hash is not relevant for init code, since it is not stored on-chain.
		let code_hash = H256::default();
		Ok(ContractBlob { code, code_info, code_hash })
	}

	/// Create a new contract from EVM runtime code.
	pub fn from_evm_runtime_code(
		code: Vec<u8>,
		owner: AccountIdOf<T>,
	) -> Result<Self, DispatchError> {
		let code_len = code.len() as u32;
		let deposit = super::calculate_code_deposit::<T>(code_len);
		Self::from_evm_runtime_code_with_deposit(code, owner, deposit)
	}

	/// Create a new contract from EVM runtime code with an explicit owner and
	/// deposit amount.
	///
	/// Used for `Origin::Root` uploads: there is no origin account to attribute
	/// the deposit to, so the caller passes the pallet's own account as a
	/// sentinel owner (no user can sign as it, so the code can't be removed via
	/// the owner-gated path) and a zero deposit (both `charge_deposit` and
	/// `refund_deposit` short-circuit at amount 0).
	pub fn from_evm_runtime_code_with_deposit(
		code: Vec<u8>,
		owner: AccountIdOf<T>,
		deposit: BalanceOf<T>,
	) -> Result<Self, DispatchError> {
		if code.len() > revm::primitives::eip170::MAX_CODE_SIZE &&
			!DebugSettings::is_unlimited_contract_size_allowed::<T>()
		{
			return Err(<Error<T>>::BlobTooLarge.into());
		}

		let code_len = code.len() as u32;

		let code_info = CodeInfo {
			owner,
			deposit,
			refcount: 0,
			code_len,
			code_type: BytecodeType::Evm,
			behaviour_version: Default::default(),
		};

		Bytecode::new_raw_checked(Bytes::from(code.to_vec())).map_err(|err| {
			log::debug!(target: LOG_TARGET, "failed to create evm bytecode from code: {err:?}" );
			<Error<T>>::CodeRejected
		})?;

		let code_hash = H256(sp_io::hashing::keccak_256(&code));
		Ok(ContractBlob { code, code_info, code_hash })
	}
}

/// Calls the EVM interpreter with the provided bytecode and inputs.
pub fn call<E: Ext>(bytecode: Bytecode, ext: &mut E, input: Vec<u8>) -> ExecResult {
	let mut interpreter = Interpreter::new(ExtBytecode::new(bytecode), input, ext);
	let tracing_enabled = tracing::if_tracing(|t| t.is_execution_tracer()).unwrap_or(false);

	let ControlFlow::Break(halt) = if tracing_enabled {
		run_plain_with_tracing(&mut interpreter)
	} else {
		run_plain(&mut interpreter)
	};
	halt.into()
}

pub fn run_plain<E: Ext>(interpreter: &mut Interpreter<E>) -> ControlFlow<Halt, Infallible> {
	loop {
		let opcode = interpreter.bytecode.opcode();
		interpreter.bytecode.relative_jump(1);
		exec_instruction(interpreter, opcode)?;
	}
}

fn run_plain_with_tracing<E: Ext>(
	interpreter: &mut Interpreter<E>,
) -> ControlFlow<Halt, Infallible> {
	loop {
		let opcode = interpreter.bytecode.opcode();
		tracing::if_tracing(|tracer| {
			let pc = interpreter.bytecode.pc() as u64;
			tracer.enter_opcode(pc, opcode, interpreter)
		});

		interpreter.bytecode.relative_jump(1);
		let res = exec_instruction(interpreter, opcode);

		tracing::if_tracing(|tracer| tracer.exit_step(interpreter, None));

		res?;
	}
}
