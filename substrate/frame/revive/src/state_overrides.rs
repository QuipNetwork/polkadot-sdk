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

//! State override application for `eth_call` dry-run simulations.
//!
//! This module applies [`StateOverrideSet`] entries to on-chain storage before a dry-run execution.
//! Because dry-runs execute inside a transaction that is always rolled back, all storage mutations
//! performed here are ephemeral and never committed to the chain.
//!
//! The supported overrides follow the
//! [Geth state override specification](https://geth.ethereum.org/docs/interacting-with-geth/rpc/objects#state-override-set):
//!
//! - **Balance**: Sets the account's spendable balance.
//! - **Nonce**: Sets the account's transaction nonce.
//! - **Code**: Injects EVM bytecode into the account, promoting an EOA to a contract if needed.
//! - **Storage (full replacement)**: Clears all existing storage slots and writes only the provided
//!   mapping.
//! - **Storage (diff)**: Patches individual slots without affecting the rest of the account's
//!   storage.

use crate::{
	AccountInfoOf, AccountType, CodeInfoOf, Config, EthTransactError, LOG_TARGET, Pallet,
	PristineCode,
	address::AddressMapper,
	evm::{StateOverride, StateOverrideSet, StorageOverride},
	exec::{Executable, Key},
	storage::{AccountInfo, ContractInfo},
	vm::ContractBlob,
};
use alloc::{format, vec::Vec};
use frame_support::traits::Get;
use sp_core::{H160, U256};
use sp_runtime::DispatchError;

/// Applies all state overrides from the given set to storage.
///
/// Each entry in the set maps an account address to overrides for that account's balance, nonce,
/// code, and/or storage. Overrides are applied independently — an account may have any
/// combination of fields overridden.
///
/// This must be called inside a dry-run transaction that will be rolled back, as the mutations are
/// written directly to storage.
pub fn apply_state_overrides<T: Config>(overrides: StateOverrideSet) -> Result<(), EthTransactError>
where
	T::Nonce: TryFrom<U256>,
{
	log::trace!(
		target: LOG_TARGET,
		"applying state overrides for {} account(s)",
		overrides.len(),
	);

	for (address, account_override) in overrides.0 {
		apply_single_account_override::<T>(address, account_override)?;
	}

	Ok(())
}

/// Applies overrides for a single account address.
///
/// Each override field is handled independently and in the following order: balance, nonce, code,
/// storage. This ordering matters because code overrides may promote an EOA to a contract, which
/// is a prerequisite for storage overrides.
fn apply_single_account_override<T: Config>(
	address: H160,
	overrides: StateOverride,
) -> Result<(), EthTransactError>
where
	T::Nonce: TryFrom<U256>,
{
	log::trace!(
		target: LOG_TARGET,
		"state override for {address:?}: balance={:?}, nonce={:?}, code={}, storage={}",
		overrides.balance,
		overrides.nonce,
		overrides.code.is_some(),
		overrides.storage.is_some(),
	);

	if let Some(balance) = overrides.balance {
		apply_balance_override::<T>(&address, balance)?;
	}

	if let Some(nonce) = overrides.nonce {
		apply_nonce_override::<T>(&address, nonce)?;
	}

	if let Some(code) = overrides.code {
		apply_code_override::<T>(&address, code.0)?;
	}

	if let Some(storage) = overrides.storage {
		apply_storage_override::<T>(&address, storage)?;
	}

	Ok(())
}

/// Overrides the balance of an account.
///
/// Delegates to [`Pallet::set_evm_balance`] which handles the native currency conversion and dust
/// accounting.
fn apply_balance_override<T: Config>(
	address: &H160,
	balance: U256,
) -> Result<(), EthTransactError> {
	Pallet::<T>::set_evm_balance(address, balance).map_err(|err| {
		EthTransactError::Message(format!("failed to override balance for {address:?}: {err:?}"))
	})
}

/// Overrides the nonce of an account.
///
/// Mutates the `frame_system` account nonce directly, which is where pallet-revive reads nonces
/// from during transaction execution.
fn apply_nonce_override<T: Config>(address: &H160, nonce: U256) -> Result<(), EthTransactError>
where
	T::Nonce: TryFrom<U256>,
{
	let nonce = nonce.try_into().map_err(|_| {
		EthTransactError::Message(format!(
			"nonce override for {address:?} exceeds the maximum representable value"
		))
	})?;
	let account_id = T::AddressMapper::to_account_id(address);
	frame_system::Account::<T>::mutate(&account_id, |account| {
		account.nonce = nonce;
	});
	Ok(())
}

/// Overrides the code of an account, promoting an EOA to a contract if necessary.
///
/// The provided bytecode is detected as either PolkaVM (if it starts with the PVM blob magic
/// bytes) or EVM runtime code, and a [`ContractBlob`] is created accordingly. If the account is
/// already a contract, its code hash is updated in place. If the account is an EOA, a new
/// [`ContractInfo`] is created for it.
///
/// The code and its metadata are stored in [`PristineCode`] and [`CodeInfoOf`] respectively, keyed
/// by the keccak-256 hash of the bytecode.
fn apply_code_override<T: Config>(address: &H160, code: Vec<u8>) -> Result<(), EthTransactError> {
	let account_id = T::AddressMapper::to_account_id(address);
	let is_pvm = code.starts_with(&polkavm_common::program::BLOB_MAGIC);
	let module = if is_pvm {
		ContractBlob::<T>::from_pvm_code(code, account_id.clone())
	} else {
		if !T::AllowEVMBytecode::get() {
			return Err(EthTransactError::Message(format!(
				"code override for {address:?} rejected: EVM bytecode is not allowed on this chain"
			)));
		}
		ContractBlob::<T>::from_evm_runtime_code(code, account_id.clone())
	}
	.map_err(|err| {
		EthTransactError::Message(format!(
			"failed to create contract blob for code override on {address:?}: {err:?}"
		))
	})?;

	let code_hash = *module.code_hash();

	if !<CodeInfoOf<T>>::contains_key(code_hash) {
		<PristineCode<T>>::insert(code_hash, module.code());
		<CodeInfoOf<T>>::insert(code_hash, module.code_info().clone());
	}

	<AccountInfoOf<T>>::try_mutate(address, |account| -> Result<(), DispatchError> {
		match account {
			Some(AccountInfo { account_type: AccountType::Contract(contract), .. }) => {
				contract.code_hash = code_hash;
			},
			_ => {
				let nonce = frame_system::Pallet::<T>::account_nonce(&account_id);
				let contract = ContractInfo::<T>::new(address, nonce, code_hash)?;
				*account = Some(AccountInfo {
					account_type: contract.into(),
					dust: account.as_ref().map(|a| a.dust).unwrap_or(0),
				});
			},
		}
		Ok(())
	})
	.map_err(|err| {
		EthTransactError::Message(format!("failed to apply code override for {address:?}: {err:?}"))
	})
}

/// Overrides storage slots of a contract account.
///
/// The account must already be a contract (either natively or via a preceding code override in the
/// same override set). Two modes are supported:
///
/// - [`StorageOverride::State`]: Clears **all** existing storage, then writes only the provided
///   slots. Slots not present in the mapping are effectively zeroed.
/// - [`StorageOverride::StateDiff`]: Writes the provided slots without clearing existing storage.
///   Slots not present in the mapping are left unchanged.
fn apply_storage_override<T: Config>(
	address: &H160,
	storage: StorageOverride,
) -> Result<(), EthTransactError> {
	let contract = AccountInfo::<T>::load_contract(address).ok_or_else(|| {
		EthTransactError::Message(format!(
			"storage override for {address:?} failed: account is not a contract"
		))
	})?;

	if let StorageOverride::State(_) = &storage {
		let _ =
			frame_support::storage::child::clear_storage(&contract.child_trie_info(), None, None);
	}

	let slots = match storage {
		StorageOverride::State(slots) | StorageOverride::StateDiff(slots) => slots,
	};

	for (key, value) in slots {
		contract
			.write(&Key::from_fixed(key.0), Some(value.0.to_vec()), None, false)
			.map_err(|err| {
				EthTransactError::Message(format!(
					"failed to write storage slot for {address:?}: {err:?}"
				))
			})?;
	}

	Ok(())
}
