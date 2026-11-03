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

//! A crate that hosts a common definitions that are relevant for the pallet-revive.

use crate::{
	BalanceOf, Config, H160, U256, deposit_payment::Funds, exec::MomentOf, mock::MockHandler,
	storage::WriteOutcome, transient_storage::TransientStorage,
};
use alloc::{boxed::Box, fmt::Debug, string::String, vec::Vec};
use codec::{Decode, Encode, MaxEncodedLen};
use core::cell::RefCell;
use frame_support::{DefaultNoBound, traits::tokens::Balance, weights::Weight};
use pallet_revive_types::runtime_api::{
	CodeV1, ContractResultV1, EthTransactInfoV1, ExecReturnValueV1, InstantiateReturnValueV1,
	StorageDepositV1,
};
use pallet_revive_uapi::ReturnFlags;
use scale_info::TypeInfo;
use sp_core::Get;
use sp_runtime::{
	DispatchError,
	traits::{One, Saturating, Zero},
};

/// Result type of a `bare_call` or `bare_instantiate` call as well as `ContractsApi::call` and
/// `ContractsApi::instantiate`.
///
/// It contains the execution result together with some auxiliary information.
///
/// #Note
///
/// It has been extended to include `events` at the end of the struct while not bumping the
/// `ContractsApi` version. Therefore when SCALE decoding a `ContractResult` its trailing data
/// should be ignored to avoid any potential compatibility issues.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct ContractResult<R, Balance> {
	/// How much weight was consumed during execution.
	pub weight_consumed: Weight,
	/// How much weight is required as weight limit in order to execute this call.
	///
	/// This value should be used to determine the weight limit for on-chain execution.
	///
	/// # Note
	///
	/// This can only be different from [`Self::weight_consumed`] when weight pre charging
	/// is used. Currently, only `seal_call_runtime` makes use of pre charging.
	/// Additionally, any `seal_call` or `seal_instantiate` makes use of pre-charging
	/// when a non-zero `weight_limit` argument is supplied.
	pub weight_required: Weight,
	/// How much balance was paid by the origin into the contract's deposit account in order to
	/// pay for storage.
	///
	/// The storage deposit is never actually charged from the origin in case of [`Self::result`]
	/// is `Err`. This is because on error all storage changes are rolled back including the
	/// payment of the deposit.
	pub storage_deposit: StorageDeposit<Balance>,
	/// The maximal storage deposit amount that occured at any time during the execution.
	/// This can be higher than the final storage_deposit due to refunds
	/// This is always a StorageDeposit::Charge(..)
	pub max_storage_deposit: StorageDeposit<Balance>,
	/// The amount of Ethereum gas that has been consumed during execution.
	pub gas_consumed: Balance,
	/// The execution result of the vm binary code.
	pub result: Result<R, DispatchError>,
}

impl<R: Default, B: Balance> Default for ContractResult<R, B> {
	fn default() -> Self {
		Self {
			weight_consumed: Default::default(),
			weight_required: Default::default(),
			storage_deposit: Default::default(),
			max_storage_deposit: Default::default(),
			gas_consumed: Default::default(),
			result: Ok(Default::default()),
		}
	}
}

impl<R, RV1, Balance> From<ContractResult<R, Balance>> for ContractResultV1<RV1, Balance>
where
	RV1: From<R>,
{
	fn from(value: ContractResult<R, Balance>) -> Self {
		Self {
			weight_consumed: value.weight_consumed,
			weight_required: value.weight_required,
			storage_deposit: value.storage_deposit.into(),
			max_storage_deposit: value.max_storage_deposit.into(),
			gas_consumed: value.gas_consumed,
			result: value.result.map(Into::into),
		}
	}
}

/// The result of the execution of a `eth_transact` call.
#[derive(Clone, Eq, PartialEq, Default, Debug)]
pub struct EthTransactInfo<Balance> {
	/// The amount of weight that was necessary to execute the transaction.
	pub weight_required: Weight,
	/// Final storage deposit charged.
	pub storage_deposit: Balance,
	/// Maximal storage deposit charged at any time during execution.
	pub max_storage_deposit: Balance,
	/// The weight and deposit equivalent in EVM Gas.
	pub eth_gas: U256,
	/// The execution return value.
	pub data: Vec<u8>,
}

impl<Balance> From<EthTransactInfo<Balance>> for EthTransactInfoV1<Balance> {
	fn from(value: EthTransactInfo<Balance>) -> Self {
		Self {
			weight_required: value.weight_required,
			storage_deposit: value.storage_deposit,
			max_storage_deposit: value.max_storage_deposit,
			eth_gas: value.eth_gas,
			data: value.data,
		}
	}
}

/// Error type of a `eth_transact` call.
#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
pub enum EthTransactError {
	Data(Vec<u8>),
	Message(String),
}

#[derive(Clone, Eq, PartialEq, Encode, Decode, Debug, TypeInfo)]
/// Error encountered while creating a BalanceWithDust from a U256 balance.
pub enum BalanceConversionError {
	/// Error encountered while creating the main balance value.
	Value,
	/// Error encountered while creating the dust value.
	Dust,
}

/// A Balance amount along with some "dust" to represent the lowest decimals that can't be expressed
/// in the native currency
#[derive(Default, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub struct BalanceWithDust<Balance> {
	/// The value expressed in the native currency
	value: Balance,
	/// The dust, representing up to 1 unit of the native currency.
	/// The dust is bounded between 0 and `crate::Config::NativeToEthRatio`
	dust: u32,
}

impl<Balance> From<Balance> for BalanceWithDust<Balance> {
	fn from(value: Balance) -> Self {
		Self { value, dust: 0 }
	}
}

impl<Balance> BalanceWithDust<Balance> {
	/// Deconstructs the `BalanceWithDust` into its components.
	pub fn deconstruct(self) -> (Balance, u32) {
		(self.value, self.dust)
	}

	/// Creates a new `BalanceWithDust` with the given value and dust.
	pub fn new_unchecked<T: Config>(value: Balance, dust: u32) -> Self {
		debug_assert!(dust < T::NativeToEthRatio::get());
		Self { value, dust }
	}

	/// Creates a new `BalanceWithDust` from the given EVM value.
	pub fn from_value<T: Config>(
		value: U256,
	) -> Result<BalanceWithDust<BalanceOf<T>>, BalanceConversionError> {
		if value.is_zero() {
			return Ok(Default::default());
		}

		let (quotient, remainder) = value.div_mod(T::NativeToEthRatio::get().into());
		let value = quotient.try_into().map_err(|_| BalanceConversionError::Value)?;
		let dust = remainder.try_into().map_err(|_| BalanceConversionError::Dust)?;

		Ok(BalanceWithDust { value, dust })
	}
}

impl<Balance: Zero + One + Saturating> BalanceWithDust<Balance> {
	/// Returns true if both the value and dust are zero.
	pub fn is_zero(&self) -> bool {
		self.value.is_zero() && self.dust == 0
	}

	/// Returns the Balance rounded to the nearest whole unit if the dust is non-zero.
	pub fn into_rounded_balance(self) -> Balance {
		if self.dust == 0 { self.value } else { self.value.saturating_add(Balance::one()) }
	}
}

/// Result type of a `bare_code_upload` call.
pub type CodeUploadResult<Balance> = Result<CodeUploadReturnValue<Balance>, DispatchError>;

/// Result type of a `get_storage` call.
pub type GetStorageResult = Result<Option<Vec<u8>>, ContractAccessError>;

/// Result type of a `set_storage` call.
pub type SetStorageResult = Result<WriteOutcome, ContractAccessError>;

/// The possible errors that can happen querying the storage of a contract.
#[derive(Copy, Clone, Eq, PartialEq, Encode, Decode, MaxEncodedLen, Debug, TypeInfo)]
pub enum ContractAccessError {
	/// The given address doesn't point to a contract.
	DoesntExist,
	/// Storage key cannot be decoded from the provided input data.
	KeyDecodingFailed,
	/// Writing to storage failed.
	StorageWriteFailed(DispatchError),
}

/// Output of a contract call or instantiation which ran to completion.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ExecReturnValue {
	/// Flags passed along by `seal_return`. Empty when `seal_return` was never called.
	pub flags: ReturnFlags,
	/// Buffer passed along by `seal_return`. Empty when `seal_return` was never called.
	pub data: Vec<u8>,
}

impl ExecReturnValue {
	/// The contract did revert all storage changes.
	pub fn did_revert(&self) -> bool {
		self.flags.contains(ReturnFlags::REVERT)
	}
}

impl From<ExecReturnValue> for ExecReturnValueV1 {
	fn from(value: ExecReturnValue) -> Self {
		Self { flags: value.flags, data: value.data }
	}
}

/// The result of a successful contract instantiation.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct InstantiateReturnValue {
	/// The output of the called constructor.
	pub result: ExecReturnValue,
	/// The address of the new contract.
	pub addr: H160,
}

impl From<InstantiateReturnValue> for InstantiateReturnValueV1 {
	fn from(value: InstantiateReturnValue) -> Self {
		Self { result: value.result.into(), addr: value.addr }
	}
}

/// The result of successfully uploading a contract.
#[derive(Clone, PartialEq, Eq, Encode, Decode, MaxEncodedLen, Debug, TypeInfo)]
pub struct CodeUploadReturnValue<Balance> {
	/// The key under which the new code is stored.
	pub code_hash: sp_core::H256,
	/// The deposit that was reserved at the caller. Is zero when the code already existed.
	pub deposit: Balance,
}

impl<Balance> From<CodeUploadReturnValue<Balance>>
	for pallet_revive_types::runtime_api::CodeUploadReturnValueV1<Balance>
{
	fn from(value: CodeUploadReturnValue<Balance>) -> Self {
		Self { code_hash: value.code_hash, deposit: value.deposit }
	}
}

/// Reference to an existing code hash or a new vm module.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum Code {
	/// A vm module as raw bytes.
	Upload(Vec<u8>),
	/// The code hash of an on-chain vm binary blob.
	Existing(sp_core::H256),
}

impl From<CodeV1> for Code {
	fn from(value: CodeV1) -> Self {
		match value {
			CodeV1::Upload(code) => Self::Upload(code),
			CodeV1::Existing(code_hash) => Self::Existing(code_hash),
		}
	}
}

/// The amount of balance that was either charged or refunded in order to pay for storage.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Debug)]
pub enum StorageDeposit<Balance> {
	/// The transaction reduced storage consumption.
	///
	/// This means that the specified amount of balance was transferred from the involved
	/// deposit accounts to the origin.
	Refund(Balance),
	/// The transaction increased storage consumption.
	///
	/// This means that the specified amount of balance was transferred from the origin
	/// to the involved deposit accounts.
	Charge(Balance),
}

impl<Balance> From<StorageDeposit<Balance>> for StorageDepositV1<Balance> {
	fn from(value: StorageDeposit<Balance>) -> Self {
		match value {
			StorageDeposit::Refund(amount) => Self::Refund(amount),
			StorageDeposit::Charge(amount) => Self::Charge(amount),
		}
	}
}

impl<T, Balance> ContractResult<T, Balance> {
	pub fn map_result<V>(self, map_fn: impl FnOnce(T) -> V) -> ContractResult<V, Balance> {
		ContractResult {
			weight_consumed: self.weight_consumed,
			weight_required: self.weight_required,
			storage_deposit: self.storage_deposit,
			max_storage_deposit: self.max_storage_deposit,
			gas_consumed: self.gas_consumed,
			result: self.result.map(map_fn),
		}
	}
}

impl<Balance: Zero> Default for StorageDeposit<Balance> {
	fn default() -> Self {
		Self::Charge(Zero::zero())
	}
}

impl<Balance: Zero + Copy> StorageDeposit<Balance> {
	/// Returns how much balance is charged or `0` in case of a refund.
	pub fn charge_or_zero(&self) -> Balance {
		match self {
			Self::Charge(amount) => *amount,
			Self::Refund(_) => Zero::zero(),
		}
	}

	pub fn is_zero(&self) -> bool {
		match self {
			Self::Charge(amount) => amount.is_zero(),
			Self::Refund(amount) => amount.is_zero(),
		}
	}
}

impl<Balance> StorageDeposit<Balance>
where
	Balance: frame_support::traits::tokens::Balance + Saturating + Ord + Copy,
{
	/// This is essentially a saturating signed add.
	pub fn saturating_add(&self, rhs: &Self) -> Self {
		use StorageDeposit::*;
		match (self, rhs) {
			(Charge(lhs), Charge(rhs)) => Charge(lhs.saturating_add(*rhs)),
			(Refund(lhs), Refund(rhs)) => Refund(lhs.saturating_add(*rhs)),
			(Charge(lhs), Refund(rhs)) => {
				if lhs >= rhs {
					Charge(lhs.saturating_sub(*rhs))
				} else {
					Refund(rhs.saturating_sub(*lhs))
				}
			},
			(Refund(lhs), Charge(rhs)) => {
				if lhs > rhs {
					Refund(lhs.saturating_sub(*rhs))
				} else {
					Charge(rhs.saturating_sub(*lhs))
				}
			},
		}
	}

	/// This is essentially a saturating signed sub.
	pub fn saturating_sub(&self, rhs: &Self) -> Self {
		use StorageDeposit::*;
		match (self, rhs) {
			(Charge(lhs), Refund(rhs)) => Charge(lhs.saturating_add(*rhs)),
			(Refund(lhs), Charge(rhs)) => Refund(lhs.saturating_add(*rhs)),
			(Charge(lhs), Charge(rhs)) => {
				if lhs >= rhs {
					Charge(lhs.saturating_sub(*rhs))
				} else {
					Refund(rhs.saturating_sub(*lhs))
				}
			},
			(Refund(lhs), Refund(rhs)) => {
				if lhs > rhs {
					Refund(lhs.saturating_sub(*rhs))
				} else {
					Charge(rhs.saturating_sub(*lhs))
				}
			},
		}
	}

	/// If the amount of deposit (this type) is constrained by a `limit` this calculates how
	/// much balance (if any) is still available from this limit.
	///
	/// # Note
	///
	/// In case of a refund the return value can be larger than `limit`.
	pub fn available(&self, limit: &Balance) -> Option<Balance> {
		use StorageDeposit::*;
		match self {
			Charge(amount) => limit.checked_sub(amount),
			Refund(amount) => Some(limit.saturating_add(*amount)),
		}
	}
}

/// `Stack` wide configuration options.
#[derive(DefaultNoBound)]
pub struct ExecConfig<T: Config> {
	/// Indicates whether the account nonce should be incremented after instantiating a new
	/// contract.
	///
	/// In Substrate, where transactions can be batched, the account's nonce should be incremented
	/// after each instantiation, ensuring that each instantiation uses a unique nonce.
	///
	/// For transactions sent from Ethereum wallets, which cannot be batched, the nonce should only
	/// be incremented once. In these cases, set this to `false` to suppress an extra nonce
	/// increment.
	///
	/// Note:
	/// The origin's nonce is already incremented pre-dispatch by the `CheckNonce` transaction
	/// extension.
	///
	/// This does not apply to contract initiated instantatiations. Those will always bump the
	/// instantiating contract's nonce.
	pub bump_nonce: bool,
	/// Whether deposits will be withdrawn from the pallet_transaction_payment credit (`Some`)
	/// free balance (`None`).
	///
	/// Contains the encoded_len + base weight.
	pub collect_deposit_from_hold: Option<(u32, Weight)>,
	/// The gas price that was chosen for this transaction.
	///
	/// It is determined when transforming `eth_transact` into a proper extrinsic.
	pub effective_gas_price: Option<U256>,
	/// Whether this configuration was created for a dry-run execution.
	/// Use to enable logic that should only run in dry-run mode.
	pub is_dry_run: Option<DryRunConfigurations<MomentOf<T>>>,
	/// An optional mock handler that can be used to override certain behaviors.
	/// This is primarily used for testing purposes and should be `None` in production
	/// environments.
	pub mock_handler: Option<Box<dyn MockHandler<T>>>,
	/// Externally supplied transient storage.
	///
	/// This is only used for testing purposes and should be `None` in production
	/// environments.
	pub test_env_transient_storage: Option<RefCell<TransientStorage<T>>>,
}

impl<T: Config> ExecConfig<T> {
	/// Create a default config appropriate when the call originated from a substrate tx.
	pub fn new_substrate_tx() -> Self {
		Self {
			bump_nonce: true,
			collect_deposit_from_hold: None,
			effective_gas_price: None,
			is_dry_run: None,
			mock_handler: None,
			test_env_transient_storage: None,
		}
	}

	pub fn new_substrate_tx_without_bump() -> Self {
		Self {
			bump_nonce: false,
			collect_deposit_from_hold: None,
			effective_gas_price: None,
			mock_handler: None,
			is_dry_run: None,
			test_env_transient_storage: None,
		}
	}

	/// Create a default config appropriate when the call originated from a ethereum tx.
	pub fn new_eth_tx(effective_gas_price: U256, encoded_len: u32, base_weight: Weight) -> Self {
		Self {
			bump_nonce: false,
			collect_deposit_from_hold: Some((encoded_len, base_weight)),
			effective_gas_price: Some(effective_gas_price),
			mock_handler: None,
			is_dry_run: None,
			test_env_transient_storage: None,
		}
	}

	/// Set this config to be a dry-run.
	pub fn with_dry_run(mut self, timestamp_override: impl Into<Option<MomentOf<T>>>) -> Self {
		self.is_dry_run =
			Some(DryRunConfigurations { timestamp_override: timestamp_override.into() });
		self
	}

	/// Classify `account` as a deposit source or refund destination based on
	/// [`Self::collect_deposit_from_hold`]: [`Funds::TxFee`] under eth-tx dispatch (where
	/// deposits flow through the tx fee pool), otherwise [`Funds::Balance`].
	pub fn funds<'a>(&self, account: &'a T::AccountId) -> Funds<'a, T::AccountId> {
		if self.collect_deposit_from_hold.is_some() {
			Funds::TxFee(account)
		} else {
			Funds::Balance(account)
		}
	}

	/// Almost clone for testing (does not clone mock_handler)
	#[cfg(test)]
	pub fn clone(&self) -> Self {
		Self {
			bump_nonce: self.bump_nonce,
			collect_deposit_from_hold: self.collect_deposit_from_hold,
			effective_gas_price: self.effective_gas_price,
			is_dry_run: self.is_dry_run.clone(),
			mock_handler: None,
			test_env_transient_storage: None,
		}
	}
}

#[derive(Clone)]
pub struct DryRunConfigurations<Moment> {
	pub timestamp_override: Option<Moment>,
}

/// Indicates whether the code was removed after the last refcount was decremented.
#[must_use = "You must handle whether the code was removed or not."]
pub enum CodeRemoved {
	/// The code was not removed. (refcount > 0)
	No,
	/// The code was removed. (refcount == 0)
	Yes,
}
