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

mod gas;
mod math;
mod storage;
mod weight;

#[cfg(test)]
mod tests;

use crate::{
	BalanceOf, Config, Error, ExecConfig, ExecOrigin as Origin, LOG_TARGET, StorageDeposit,
	evm::fees::InfoT, exec::CallResources, storage::ContractInfo, vm::evm::Halt,
};

pub use gas::SignedGas;
pub use storage::Diff;
pub use weight::{ChargedAmount, Token};

use frame_support::{DebugNoBound, DefaultNoBound};
use num_traits::Zero;

use core::{fmt::Debug, marker::PhantomData, ops::ControlFlow};
use sp_runtime::{FixedPointNumber, Weight};
use storage::{DepositOf, GenericMeter as GenericStorageMeter, Meter as RootStorageMeter};
use weight::WeightMeter;

use sp_runtime::{DispatchError, DispatchResult, FixedU128, SaturatedConversion};

/// A type-state pattern ensuring that meters can only be used in valid states (root vs nested).
///
/// It is sealed and cannot be implemented outside of this module.
pub trait State: private::Sealed + Default + Debug {}

/// Root state for transaction-level resource metering.
///
/// Represents the top-level accounting of a transaction's resource usage.
#[derive(Default, Debug)]
pub struct Root;

/// Nested state for frame-level resource metering.
///
/// Represents resource accounting for a single call frame.
#[derive(Default, Debug)]
pub struct Nested;

impl State for Root {}
impl State for Nested {}

mod private {
	pub trait Sealed {}
	impl Sealed for super::Root {}
	impl Sealed for super::Nested {}
}

/// The type of resource meter used at the root level for transactions as a whole.
pub type TransactionMeter<T> = ResourceMeter<T, Root>;
/// The type of resource meter used for an execution frame.
pub type FrameMeter<T> = ResourceMeter<T, Nested>;

/// Snapshot of a [`ResourceMeter`]'s consumption at a point in time.
///
/// Produced by [`ResourceMeter::snapshot`] and consumed by [`ResourceMeter::delta_since`].
pub struct MeterSnapshot<T: Config> {
	weight: Weight,
	gas: SignedGas<T>,
}

/// Resource meter tracking weight and storage deposit consumption.
#[derive(DefaultNoBound)]
pub struct ResourceMeter<T: Config, S: State> {
	/// The weight meter. Tracks consumed weight and weight limits.
	weight: WeightMeter<T>,

	/// The deposit meter. Tracks consumed storage deposit and storage deposit limits.
	deposit: GenericStorageMeter<T, S>,

	/// This is the maximum total consumable gas.
	///
	/// It is the sum of a) the total consumed gas (i.e., including all previous frames) at the
	/// time the frame started and b) the gas limit of the frame. We don't store the gas limit of
	/// the frame separately, it can be derived from `max_total_gas` by subtracting the total gas
	/// at the beginning of the frame.
	///
	/// `max_total_gas` is only required for Ethereum execution, it is always zero for Substrate
	/// executions.
	max_total_gas: SignedGas<T>,

	/// The total consumed weight at the time the frame started.
	total_consumed_weight_before: Weight,

	/// The total consumed storage deposit at the time the frame started.
	total_consumed_deposit_before: DepositOf<T>,

	/// The limits defined for the transaction. This determines whether this transaction uses the
	/// Ethereum or Substrate execution mode.
	transaction_limits: TransactionLimits<T>,

	_phantom: PhantomData<S>,
}

/// Transaction-wide resource limit configuration.
///
/// Represents the two supported resource accounting modes:
/// - EthereumGas: Single gas limit
/// - WeightAndDeposit: Explicit limits for both computational weight and storage deposit
#[derive(DebugNoBound, Clone)]
pub enum TransactionLimits<T: Config> {
	/// Ethereum execution mode: the transaction only specifies a gas limit.
	EthereumGas {
		/// The Ethereum gas limit
		eth_gas_limit: BalanceOf<T>,
		/// The weight limit for this transaction. This ensures that execution will not exhaust
		/// weight limit. This is required for eth_transact extrinsic execution to ensure that the
		/// max extrinsic weights is not overstepped.
		weight_limit: Weight,
		/// Some extra information about the transaction that is required to calculate gas usage.
		eth_tx_info: EthTxInfo<T>,
	},
	/// Substrate execution mode: the transaction specifies a weight limit and a storage deposit
	/// limit
	WeightAndDeposit { weight_limit: Weight, deposit_limit: BalanceOf<T> },
}

impl<T: Config> Default for TransactionLimits<T> {
	fn default() -> Self {
		Self::WeightAndDeposit {
			weight_limit: Default::default(),
			deposit_limit: Default::default(),
		}
	}
}

impl<T: Config, S: State> ResourceMeter<T, S> {
	/// Create a new nested meter with derived resource limits.
	pub fn new_nested(&self, limit: &CallResources<T>) -> Result<FrameMeter<T>, DispatchError> {
		log::trace!(
			target: LOG_TARGET,
			"Creating nested meter from parent: \
				limit={limit:?}, \
				weight_left={:?}, \
				deposit_left={:?}, \
				weight_consumed={:?}, \
				deposit_consumed={:?}",
			self.weight_left(),
			self.deposit_left(),
			self.weight_consumed(),
			self.deposit_consumed(),
		);

		let mut new_meter = match &self.transaction_limits {
			TransactionLimits::EthereumGas { eth_tx_info, .. } => {
				math::ethereum_execution::new_nested_meter(self, limit, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { .. } => {
				math::substrate_execution::new_nested_meter(self, limit)
			},
		}?;

		new_meter.adjust_effective_weight_limit()?;

		log::trace!(
			target: LOG_TARGET,
			"Creating nested meter done: \
				weight_left={:?}, \
				deposit_left={:?}, \
				weight_consumed={:?}, \
				deposit_consumed={:?}",
			new_meter.weight_left(),
			new_meter.deposit_left(),
			new_meter.weight_consumed(),
			new_meter.deposit_consumed(),
		);

		Ok(new_meter)
	}

	/// Absorb only the weight consumption from a nested frame meter.
	pub fn absorb_weight_meter_only(&mut self, other: FrameMeter<T>) {
		log::trace!(
			target: LOG_TARGET,
			"Absorb weight meter only: \
				parent_weight_left={:?}, \
				parent_deposit_left={:?}, \
				parent_weight_consumed={:?}, \
				parent_deposit_consumed={:?}, \
				child_weight_left={:?}, \
				child_deposit_left={:?}, \
				child_weight_consumed={:?}, \
				child_deposit_consumed={:?}",
			self.weight_left(),
			self.deposit_left(),
			self.weight_consumed(),
			self.deposit_consumed(),
			other.weight_left(),
			other.deposit_left(),
			other.weight_consumed(),
			other.deposit_consumed(),
		);

		self.weight.absorb_nested(other.weight);
		self.deposit.absorb_only_max_charged(other.deposit);

		log::trace!(
			target: LOG_TARGET,
			"Absorb weight meter done: \
				parent_weight_left={:?}, \
				parent_deposit_left={:?}, \
				parent_weight_consumed={:?}, \
				parent_deposit_consumed={:?}",
			self.weight_left(),
			self.deposit_left(),
			self.weight_consumed(),
			self.deposit_consumed(),
		);
	}

	/// Absorb all resource consumption from a nested frame meter.
	pub fn absorb_all_meters(
		&mut self,
		other: FrameMeter<T>,
		contract: &T::AccountId,
		info: Option<&mut ContractInfo<T>>,
	) {
		log::trace!(
			target: LOG_TARGET,
			"Absorb all meters: \
				parent_weight_left={:?}, \
				parent_deposit_left={:?}, \
				parent_weight_consumed={:?}, \
				parent_deposit_consumed={:?}, \
				child_weight_left={:?}, \
				child_deposit_left={:?}, \
				child_weight_consumed={:?}, \
				child_deposit_consumed={:?}",
			self.weight_left(),
			self.deposit_left(),
			self.weight_consumed(),
			self.deposit_consumed(),
			other.weight_left(),
			other.deposit_left(),
			other.weight_consumed(),
			other.deposit_consumed(),
		);

		self.weight.absorb_nested(other.weight);
		self.deposit.absorb(other.deposit, contract, info);

		let result = self.adjust_effective_weight_limit();
		debug_assert!(result.is_ok(), "Absorbing nested meters should not exceed limits");

		log::trace!(
			target: LOG_TARGET,
			"Absorb all meters done: \
				parent_weight_left={:?}, \
				parent_deposit_left={:?}, \
				parent_weight_consumed={:?}, \
				parent_deposit_consumed={:?}",
			self.weight_left(),
			self.deposit_left(),
			self.weight_consumed(),
			self.deposit_consumed(),
		);
	}

	/// Charge a weight token against this meter's remaining weight limit.
	///
	/// Returns `Err(Error::OutOfGas)` if the weight limit would be exceeded.
	#[inline]
	pub fn charge_weight_token<Tok: Token<T>>(
		&mut self,
		token: Tok,
	) -> Result<ChargedAmount, DispatchError> {
		self.weight.charge(token)
	}

	/// Try to charge a weight token or halt if not enough weight is left.
	#[inline]
	pub fn charge_or_halt<Tok: Token<T>>(
		&mut self,
		token: Tok,
	) -> ControlFlow<Halt, ChargedAmount> {
		self.weight.charge_or_halt(token)
	}

	/// Adjust an earlier weight charge with the actual weight consumed.
	pub fn adjust_weight<Tok: Token<T>>(&mut self, charged_amount: ChargedAmount, token: Tok) {
		self.weight.adjust_weight(charged_amount, token);
	}

	/// Synchronize meter state with PolkaVM executor's fuel consumption.
	///
	/// Maps the VM's internal fuel accounting to weight consumption:
	/// - Converts engine fuel units to weight units
	/// - Updates meter state to match actual VM resource usage
	pub fn sync_from_executor(&mut self, engine_fuel: polkavm::Gas) -> Result<(), DispatchError> {
		self.weight.sync_from_executor(engine_fuel)
	}

	/// Convert meter state to PolkaVM executor fuel units.
	///
	/// Prepares for VM execution by:
	/// - Computing remaining available weight
	/// - Converting weight units to VM fuel units and return
	pub fn sync_to_executor(&mut self) -> polkavm::Gas {
		self.weight.sync_to_executor()
	}

	/// Consume all remaining weight in the meter.
	pub fn consume_all_weight(&mut self) {
		self.weight.consume_all();
	}

	/// Record a storage deposit charge against this meter.
	pub fn charge_deposit(&mut self, deposit: &DepositOf<T>) -> DispatchResult {
		log::trace!(
			target: LOG_TARGET,
			"Charge deposit: \
				deposit={:?}, \
				deposit_left={:?}, \
				deposit_consumed={:?}, \
				max_charged={:?}",
			deposit,
			self.deposit_left(),
			self.deposit_consumed(),
			self.deposit.max_charged(),
		);

		if let StorageDeposit::Charge(amount) = deposit {
			if self.deposit.is_root && self.deposit_left().map_or(true, |left| left < *amount) {
				return Err(<Error<T>>::StorageDepositLimitExhausted.into());
			}
		}

		self.deposit.record_charge(deposit);
		self.adjust_effective_weight_limit()
	}

	/// Get remaining ethereum gas equivalent.
	///
	/// Converts remaining resources to ethereum gas units:
	/// - For ethereum mode: computes directly from gas accounting
	/// - For substrate mode: converts weight+deposit to gas equivalent
	/// Returns None if resources are exhausted or conversion fails.
	pub fn eth_gas_left(&self) -> Option<BalanceOf<T>> {
		let gas_left = match &self.transaction_limits {
			TransactionLimits::EthereumGas { eth_tx_info, .. } => {
				math::ethereum_execution::gas_left(self, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { .. } => math::substrate_execution::gas_left(self),
		}?;

		gas_left.to_ethereum_gas()
	}

	/// Get remaining weight available.
	///
	/// Computes remaining computational capacity:
	/// - For ethereum mode: converts from gas to weight units
	/// - For substrate mode: subtracts consumed from weight limit
	/// Returns None if resources are exhausted.
	pub fn weight_left(&self) -> Option<Weight> {
		match &self.transaction_limits {
			TransactionLimits::EthereumGas { eth_tx_info, .. } => {
				math::ethereum_execution::weight_left(self, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { .. } => {
				math::substrate_execution::weight_left(self)
			},
		}
	}

	/// Get remaining deposit available.
	///
	/// Computes remaining storage deposit allowance:
	/// - For ethereum mode: converts from gas to deposit units
	/// - For substrate mode: subtracts consumed from deposit limit
	/// Returns None if resources are exhausted.
	pub fn deposit_left(&self) -> Option<BalanceOf<T>> {
		match &self.transaction_limits {
			TransactionLimits::EthereumGas { eth_tx_info, .. } => {
				math::ethereum_execution::deposit_left(self, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { .. } => {
				math::substrate_execution::deposit_left(self)
			},
		}
	}

	/// Calculate total gas consumed so far.
	///
	/// Computes the ethereum-gas equivalent of all resource usage:
	/// - Converts weight and deposit consumption to gas units
	/// - For ethereum mode: uses direct gas accounting
	/// - For substrate mode: synthesizes from weight+deposit usage
	pub fn total_consumed_gas(&self) -> BalanceOf<T> {
		let signed_gas = match &self.transaction_limits {
			TransactionLimits::EthereumGas { eth_tx_info, .. } => {
				math::ethereum_execution::total_consumed_gas(self, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { .. } => {
				math::substrate_execution::total_consumed_gas(self)
			},
		};

		signed_gas.to_ethereum_gas().unwrap_or_default()
	}

	/// Get total weight consumed
	pub fn weight_consumed(&self) -> Weight {
		self.weight.weight_consumed()
	}

	/// Get total weight required
	/// This is the maximum amount of weight consumption that occurred during execution so far
	/// This is relevant because consumed weight can decrease in case it is asjusted a posteriori
	/// for some operations
	pub fn weight_required(&self) -> Weight {
		self.weight.weight_required()
	}

	/// Get total storage deposit consumed in the current frame.
	///
	/// Returns the net storage deposit change from this frame,
	pub fn deposit_consumed(&self) -> DepositOf<T> {
		self.deposit.consumed()
	}

	/// Get maximum storage deposit required at any point.
	///
	/// Returns the highest deposit amount needed during execution,
	/// accounting for temporary storage spikes before later refunds.
	pub fn deposit_required(&self) -> DepositOf<T> {
		self.deposit.max_charged()
	}

	/// Get the Ethereum gas that has been consumed during the lifetime of this meter
	pub fn eth_gas_consumed(&self) -> BalanceOf<T> {
		self.eth_gas_consumed_signed().to_ethereum_gas().unwrap_or_default()
	}

	/// Same as [`Self::eth_gas_consumed`] but returns the unrounded [`SignedGas`].
	///
	/// Prefer this when computing a delta across two snapshots: subtracting in [`SignedGas`] form
	/// avoids the double ceil-rounding that [`Self::eth_gas_consumed`] performs at each call.
	pub fn eth_gas_consumed_signed(&self) -> SignedGas<T> {
		match &self.transaction_limits {
			TransactionLimits::EthereumGas { eth_tx_info, .. } => {
				math::ethereum_execution::eth_gas_consumed(self, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { .. } => {
				math::substrate_execution::eth_gas_consumed(self)
			},
		}
	}

	/// Take a snapshot of the meter's current consumption for later use with
	/// [`Self::delta_since`].
	pub fn snapshot(&self) -> MeterSnapshot<T> {
		MeterSnapshot { weight: self.weight_consumed(), gas: self.eth_gas_consumed_signed() }
	}

	/// Ethereum gas and weight consumed since `snapshot` was taken.
	///
	/// Gas subtraction happens in [`SignedGas`] form so that the ceil-rounding inside
	/// `to_ethereum_gas` is applied once to the delta, not to each snapshot.
	pub fn delta_since(&self, snapshot: &MeterSnapshot<T>) -> (u64, Weight) {
		let gas = self
			.eth_gas_consumed_signed()
			.saturating_sub(&snapshot.gas)
			.to_ethereum_gas()
			.unwrap_or_default()
			.try_into()
			.unwrap_or(u64::MAX);
		let weight = self.weight_consumed().saturating_sub(snapshot.weight);
		(gas, weight)
	}

	/// Determine and set the new effective weight limit of the weight meter.
	///
	/// This function needs to be called whenever there is a change in the deposit meter. It is a
	/// function of `ResourceMeter` instead of `WeightMeter` because its outcome also depends on the
	/// consumed storage deposits.
	fn adjust_effective_weight_limit(&mut self) -> DispatchResult {
		if matches!(self.transaction_limits, TransactionLimits::WeightAndDeposit { .. }) {
			return Ok(());
		}

		if let Some(weight_left) = self.weight_left() {
			let new_effective_limit = self.weight.weight_consumed().saturating_add(weight_left);
			self.weight.set_effective_weight_limit(new_effective_limit);
			Ok(())
		} else {
			Err(<Error<T>>::OutOfGas.into())
		}
	}
}

impl<T: Config> TransactionMeter<T> {
	/// Create a new transaction-level meter with the specified resource limits.
	///
	/// Initializes either:
	/// - An ethereum-style gas-based meter or
	/// - A substrate-style meter with explicit weight and deposit limits
	pub fn new(transaction_limits: TransactionLimits<T>) -> Result<Self, DispatchError> {
		log::debug!(
			target: LOG_TARGET,
			"Start new meter: transaction_limits={transaction_limits:?}",
		);

		let mut transaction_meter = match transaction_limits {
			TransactionLimits::EthereumGas { eth_gas_limit, weight_limit, eth_tx_info } => {
				math::ethereum_execution::new_root(eth_gas_limit, weight_limit, eth_tx_info)
			},
			TransactionLimits::WeightAndDeposit { weight_limit, deposit_limit } => {
				math::substrate_execution::new_root(weight_limit, deposit_limit)
			},
		}?;

		transaction_meter.adjust_effective_weight_limit()?;

		log::trace!(
			target: LOG_TARGET,
			"New meter done: \
				weight_left={:?}, \
				deposit_left={:?}, \
				weight_consumed={:?}, \
				deposit_consumed={:?}",
			transaction_meter.weight_left(),
			transaction_meter.deposit_left(),
			transaction_meter.weight_consumed(),
			transaction_meter.deposit_consumed(),
		);

		Ok(transaction_meter)
	}

	/// Convenience constructor for substrate-style weight+deposit limits.
	pub fn new_from_limits(
		weight_limit: Weight,
		deposit_limit: BalanceOf<T>,
	) -> Result<Self, DispatchError> {
		Self::new(TransactionLimits::WeightAndDeposit { weight_limit, deposit_limit })
	}

	/// Execute all postponed storage deposit operations.
	///
	/// Returns `Err(Error::StorageDepositNotEnoughFunds)` if deposit limit would be exceeded.
	pub fn execute_postponed_deposits(
		&mut self,
		origin: &Origin<T>,
		exec_config: &ExecConfig<T>,
	) -> Result<DepositOf<T>, DispatchError> {
		log::debug!(
			target: LOG_TARGET,
			"Transaction meter finishes: \
				weight_left={:?}, \
				deposit_left={:?}, \
				weight_consumed={:?}, \
				deposit_consumed={:?}, \
				eth_gas_consumed={:?}",
			self.weight_left(),
			self.deposit_left(),
			self.weight_consumed(),
			self.deposit_consumed(),
			self.eth_gas_consumed(),
		);

		if self.deposit_left().is_none() {
			// Deposit limit exceeded
			return Err(<Error<T>>::StorageDepositNotEnoughFunds.into());
		}

		self.deposit.execute_postponed_deposits(origin, exec_config)
	}

	/// Mark a contract as terminated
	///
	/// This will signal to the meter to discard all charged and refunds incured by this
	/// contract. Furthermore it will record that there was a refund of `refunded` and adapt the
	/// total deposit accordingly
	pub fn terminate(&mut self, contract_account: T::AccountId, refunded: BalanceOf<T>) {
		self.deposit.terminate(contract_account, refunded);
	}
}

impl<T: Config> FrameMeter<T> {
	/// Record a contract's storage deposit and schedule the transfer.
	///
	/// Updates the frame's deposit accounting and schedules the actual token transfer
	/// for later execution – at the end of the transaction execution.
	pub fn charge_contract_deposit_and_transfer(
		&mut self,
		contract: T::AccountId,
		amount: DepositOf<T>,
	) -> DispatchResult {
		log::trace!(
			target: LOG_TARGET,
			"Charge deposit and transfer: \
				amount={:?}, \
				deposit_left={:?}, \
				deposit_consumed={:?}, \
				max_charged={:?}",
			amount,
			self.deposit_left(),
			self.deposit_consumed(),
			self.deposit.max_charged(),
		);

		self.deposit.charge_deposit(contract, amount);
		self.adjust_effective_weight_limit()
	}

	/// Record storage changes of a contract.
	pub fn record_contract_storage_changes(&mut self, diff: &Diff) -> DispatchResult {
		log::trace!(
			target: LOG_TARGET,
			"Charge contract storage: \
				diff={:?}, \
				deposit_left={:?}, \
				deposit_consumed={:?}, \
				max_charged={:?}",
			diff,
			self.deposit_left(),
			self.deposit_consumed(),
			self.deposit.max_charged(),
		);

		self.deposit.charge(diff);
		self.adjust_effective_weight_limit()
	}

	/// [`Self::charge_contract_deposit_and_transfer`] and [`Self::record_contract_storage_changes`]
	/// does not enforce the storage limit since we want to do this check as late as possible to
	/// allow later refunds to offset earlier charges.
	pub fn finalize(&mut self, info: Option<&mut ContractInfo<T>>) -> DispatchResult {
		self.deposit.finalize_own_contributions(info);

		if self.deposit_left().is_none() {
			return Err(<Error<T>>::StorageDepositLimitExhausted.into());
		}

		Ok(())
	}

	/// Apply pending storage changes to a ContractInfo without finalizing the meter.
	///
	/// This is used before creating a nested frame to ensure the child frame can see
	/// the parent's pending storage changes when calculating refunds. This fixes the issue
	/// where storage deposit refunds fail in subframes because the parent's pending
	/// charges haven't been committed to ContractInfo yet.
	///
	/// See: <https://github.com/paritytech/contract-issues/issues/213>
	pub fn apply_pending_storage_changes(&self, info: &mut ContractInfo<T>) {
		self.deposit.apply_pending_changes_to_contract(info);
	}

	/// See [`storage::RawMeter::bank_pending_changes`].
	pub fn bank_pending_storage_changes(
		&mut self,
		contract: T::AccountId,
		info: &mut ContractInfo<T>,
	) {
		self.deposit.bank_pending_changes(contract, info);
	}
}

/// Ethereum transaction context for gas conversions.
///
/// Contains the parameters needed to convert between ethereum gas and substrate resources
/// (weight/deposit)
#[derive(DebugNoBound, Clone)]
pub struct EthTxInfo<T: Config> {
	/// The encoding length of the extrinsic
	pub encoded_len: u32,
	/// The extra weight of the transaction. The total weight of the extrinsic is `extra_weight` +
	/// the weight consumed during smart contract execution.
	pub extra_weight: Weight,
	_phantom: PhantomData<T>,
}

impl<T: Config> EthTxInfo<T> {
	/// Create a new ethereum transaction context with the given parameters.
	pub fn new(encoded_len: u32, extra_weight: Weight) -> Self {
		Self { encoded_len, extra_weight, _phantom: PhantomData }
	}

	/// Calculate total gas consumed by weight and storage operations.
	pub fn gas_consumption(
		&self,
		consumed_weight: &Weight,
		consumed_deposit: &DepositOf<T>,
	) -> SignedGas<T> {
		let fixed_fee = T::FeeInfo::fixed_fee(self.encoded_len);
		let deposit_and_fixed_fee =
			consumed_deposit.saturating_add(&DepositOf::<T>::Charge(fixed_fee));
		let deposit_gas = SignedGas::from_adjusted_deposit_charge(&deposit_and_fixed_fee);

		let weight_gas = SignedGas::from_weight_fee(T::FeeInfo::weight_to_fee(
			&consumed_weight.saturating_add(self.extra_weight),
		));

		deposit_gas.saturating_add(&weight_gas)
	}

	/// Calculate maximal possible remaining weight that can be consumed given a particular gas
	/// limit.
	///
	/// Returns None if remaining gas would not allow any more weight consumption.
	pub fn weight_remaining(
		&self,
		max_total_gas: &SignedGas<T>,
		total_weight_consumption: &Weight,
		total_deposit_consumption: &DepositOf<T>,
	) -> Option<Weight> {
		let fixed_fee = T::FeeInfo::fixed_fee(self.encoded_len);
		let deposit_and_fixed_fee =
			total_deposit_consumption.saturating_add(&DepositOf::<T>::Charge(fixed_fee));
		let deposit_gas = SignedGas::from_adjusted_deposit_charge(&deposit_and_fixed_fee);

		let consumable_fee = max_total_gas.saturating_sub(&deposit_gas).to_weight_fee()?;

		T::FeeInfo::fee_to_weight(consumable_fee)
			.checked_sub(&total_weight_consumption.saturating_add(self.extra_weight))
	}
}
