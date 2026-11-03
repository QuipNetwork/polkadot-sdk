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

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use core::marker::PhantomData;
use ethereum_standards::{
	IERC20,
	IERC20::{IERC20Calls, IERC20Events},
};
use frame_support::traits::fungibles::metadata::Inspect as MetadataInspect;
use pallet_assets::{weights::WeightInfo as _, Call, Config, TransferFlags};
use pallet_revive::precompiles::{
	alloy::{
		self,
		primitives::IntoLogData,
		sol_types::{Revert, SolCall},
	},
	AddressMapper, AddressMatcher, Error, Ext, Precompile, RuntimeCosts, H160, H256,
};
use sp_runtime::traits::{UniqueSaturatedInto, Zero};

pub mod foreign_assets;
pub mod migration;
pub mod permit;
pub mod weights;
pub use weights::WeightInfo;

#[cfg(feature = "runtime-benchmarks")]
pub(crate) mod benchmarking;

#[cfg(test)]
mod foreign_assets_tests;
#[cfg(test)]
mod migration_tests;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod permit_precompile_tests;
#[cfg(test)]
mod permit_tests;
#[cfg(test)]
mod test_helpers;
#[cfg(test)]
mod tests;

pub use foreign_assets::{pallet, pallet::Config as ForeignAssetsConfig, ForeignAssetId};
pub use migration::MigrateForeignAssetPrecompileMappings;
pub use permit::pallet::Config as PermitConfig;

/// Means of extracting the asset id from the precompile address.
pub trait AssetIdExtractor {
	type AssetId;
	/// Extracts the asset id from the address.
	fn asset_id_from_address(address: &[u8; 20]) -> Result<Self::AssetId, Error>;
}

/// The configuration of a pallet-assets precompile.
pub trait AssetPrecompileConfig {
	/// The Address matcher used by the precompile.
	const MATCHER: AddressMatcher;

	/// The [`AssetIdExtractor`] used by the precompile.
	type AssetIdExtractor: AssetIdExtractor;
}

/// An `AssetIdExtractor` that stores the asset id directly inside the address.
pub struct InlineAssetIdExtractor;

impl AssetIdExtractor for InlineAssetIdExtractor {
	type AssetId = u32;
	fn asset_id_from_address(addr: &[u8; 20]) -> Result<Self::AssetId, Error> {
		let bytes: [u8; 4] = addr[0..4].try_into().expect("slice is 4 bytes; qed");
		let index = u32::from_be_bytes(bytes);
		Ok(index)
	}
}

/// A precompile configuration that uses a prefix [`AddressMatcher`].
pub struct InlineIdConfig<const PREFIX: u16>;

impl<const P: u16> AssetPrecompileConfig for InlineIdConfig<P> {
	const MATCHER: AddressMatcher = AddressMatcher::Prefix(core::num::NonZero::new(P).unwrap());
	type AssetIdExtractor = InlineAssetIdExtractor;
}

/// An `AssetIdExtractor` that maps a local asset id (4 bytes taken from the address) to a foreign
/// asset id.
pub struct ForeignAssetIdExtractor<Runtime, Instance = ()> {
	_phantom: PhantomData<(Runtime, Instance)>,
}

impl<Runtime, Instance: 'static> AssetIdExtractor for ForeignAssetIdExtractor<Runtime, Instance>
where
	Runtime: pallet_assets::Config<Instance>
		+ pallet::Config<ForeignAssetId = <Runtime as pallet_assets::Config<Instance>>::AssetId>
		+ pallet_revive::Config,
{
	type AssetId = <Runtime as pallet_assets::Config<Instance>>::AssetId;
	fn asset_id_from_address(addr: &[u8; 20]) -> Result<Self::AssetId, Error> {
		let bytes: [u8; 4] = addr[0..4].try_into().expect("slice is 4 bytes; qed");
		let index = u32::from_be_bytes(bytes);
		pallet::Pallet::<Runtime>::asset_id_of(index)
			.ok_or(Error::Revert(Revert { reason: "Invalid foreign asset id".into() }))
	}
}

/// A precompile configuration that uses a prefix [`AddressMatcher`].
pub struct ForeignIdConfig<const PREFIX: u16, Runtime, Instance = ()> {
	_phantom: PhantomData<(Runtime, Instance)>,
}

impl<const P: u16, Runtime, Instance: 'static> AssetPrecompileConfig
	for ForeignIdConfig<P, Runtime, Instance>
where
	Runtime: pallet_assets::Config<Instance>
		+ pallet::Config<ForeignAssetId = <Runtime as pallet_assets::Config<Instance>>::AssetId>
		+ pallet_revive::Config,
{
	const MATCHER: AddressMatcher = AddressMatcher::Prefix(core::num::NonZero::new(P).unwrap());
	type AssetIdExtractor = ForeignAssetIdExtractor<Runtime, Instance>;
}

/// An ERC20 precompile with EIP-2612 permit support.
pub struct ERC20<Runtime, PrecompileConfig, Instance = ()> {
	_phantom: PhantomData<(Runtime, PrecompileConfig, Instance)>,
}

impl<Runtime, PrecompileConfig, Instance: 'static> Precompile
	for ERC20<Runtime, PrecompileConfig, Instance>
where
	PrecompileConfig: AssetPrecompileConfig,
	Runtime: crate::Config<Instance> + pallet_revive::Config + permit::Config,
	<<PrecompileConfig as AssetPrecompileConfig>::AssetIdExtractor as AssetIdExtractor>::AssetId:
		Into<<Runtime as Config<Instance>>::AssetId>,
	Call<Runtime, Instance>: Into<<Runtime as pallet_revive::Config>::RuntimeCall>,
	alloy::primitives::U256: TryInto<<Runtime as Config<Instance>>::Balance>,
	alloy::primitives::U256: TryFrom<<Runtime as Config<Instance>>::Balance>,
{
	type T = Runtime;
	type Interface = IERC20::IERC20Calls;
	const MATCHER: AddressMatcher = PrecompileConfig::MATCHER;
	const HAS_CONTRACT_INFO: bool = false;

	fn call(
		address: &[u8; 20],
		input: &Self::Interface,
		env: &mut impl Ext<T = Self::T>,
	) -> Result<Vec<u8>, Error> {
		frame_support::ensure!(
			!env.is_delegate_call(),
			pallet_revive::Error::<Self::T>::PrecompileDelegateDenied,
		);

		let asset_id = PrecompileConfig::AssetIdExtractor::asset_id_from_address(address)?.into();
		let contract_addr = H160::from(*address);

		match input {
			// State-changing calls - check read-only
			IERC20Calls::transfer(_) |
			IERC20Calls::approve(_) |
			IERC20Calls::transferFrom(_) |
			IERC20Calls::permit(_)
				if env.is_read_only() =>
			{
				Err(Error::Error(pallet_revive::Error::<Self::T>::StateChangeDenied.into()))
			},

			// ERC20 functions
			IERC20Calls::transfer(call) => Self::transfer(asset_id, call, env),
			IERC20Calls::totalSupply(_) => Self::total_supply(asset_id, env),
			IERC20Calls::balanceOf(call) => Self::balance_of(asset_id, call, env),
			IERC20Calls::allowance(call) => Self::allowance(asset_id, call, env),
			IERC20Calls::approve(call) => Self::approve(asset_id, call, env),
			IERC20Calls::transferFrom(call) => Self::transfer_from(asset_id, call, env),

			// ERC20Permit functions (EIP-2612)
			IERC20Calls::permit(call) => Self::permit(asset_id, contract_addr, call, env),
			IERC20Calls::nonces(call) => Self::nonces(contract_addr, call, env),
			IERC20Calls::DOMAIN_SEPARATOR(_) => {
				Self::domain_separator(asset_id, contract_addr, env)
			},

			// ERC20Metadata functions
			IERC20Calls::name(_) => Self::name(asset_id, env),
			IERC20Calls::symbol(_) => Self::symbol(asset_id, env),
			IERC20Calls::decimals(_) => Self::decimals(asset_id, env),
		}
	}
}

const ERR_INVALID_CALLER: &str = "Invalid caller";
const ERR_BALANCE_CONVERSION_FAILED: &str = "Balance conversion failed";

impl<Runtime, PrecompileConfig, Instance: 'static> ERC20<Runtime, PrecompileConfig, Instance>
where
	PrecompileConfig: AssetPrecompileConfig,
	Runtime: crate::Config<Instance> + pallet_revive::Config + permit::Config,
	<<PrecompileConfig as AssetPrecompileConfig>::AssetIdExtractor as AssetIdExtractor>::AssetId:
		Into<<Runtime as Config<Instance>>::AssetId>,
	Call<Runtime, Instance>: Into<<Runtime as pallet_revive::Config>::RuntimeCall>,
	alloy::primitives::U256: TryInto<<Runtime as Config<Instance>>::Balance>,
	alloy::primitives::U256: TryFrom<<Runtime as Config<Instance>>::Balance>,
{
	/// Get the caller as an `H160` address.
	fn caller(env: &mut impl Ext<T = Runtime>) -> Result<H160, Error> {
		env.caller()
			.account_id()
			.map(<Runtime as pallet_revive::Config>::AddressMapper::to_address)
			.map_err(|_| Error::Revert(Revert { reason: ERR_INVALID_CALLER.into() }))
	}

	/// Convert a `U256` value to the balance type of the pallet.
	fn to_balance(
		value: alloy::primitives::U256,
	) -> Result<<Runtime as Config<Instance>>::Balance, Error> {
		value
			.try_into()
			.map_err(|_| Error::Revert(Revert { reason: ERR_BALANCE_CONVERSION_FAILED.into() }))
	}

	/// Convert a balance to a `U256` value.
	/// Note: this is needed because `From` is not implemented for unsigned integer types.
	fn to_u256(
		value: <Runtime as Config<Instance>>::Balance,
	) -> Result<alloy::primitives::U256, Error> {
		alloy::primitives::U256::try_from(value)
			.map_err(|_| Error::Revert(Revert { reason: ERR_BALANCE_CONVERSION_FAILED.into() }))
	}

	/// Deposit an event to the runtime.
	fn deposit_event(env: &mut impl Ext<T = Runtime>, event: IERC20Events) -> Result<(), Error> {
		let (topics, data) = event.into_log_data().split();
		let topics = topics.into_iter().map(|v| H256(v.0)).collect::<Vec<_>>();
		env.frame_meter_mut().charge_weight_token(RuntimeCosts::DepositEvent {
			num_topic: topics.len() as u32,
			len: data.len() as u32,
		})?;
		env.deposit_event(topics, data.to_vec());
		Ok(())
	}

	/// Execute the transfer call.
	fn transfer(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		call: &IERC20::transferCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::transfer())?;

		let from = Self::caller(env)?;
		let dest = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(
			&call.to.into_array().into(),
		);

		let f = TransferFlags { keep_alive: false, best_effort: false, burn_dust: false };
		pallet_assets::Pallet::<Runtime, Instance>::do_transfer(
			asset_id,
			&<Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&from),
			&dest,
			Self::to_balance(call.value)?,
			None,
			f,
		)?;

		Self::deposit_event(
			env,
			IERC20Events::Transfer(IERC20::Transfer {
				from: from.0.into(),
				to: call.to,
				value: call.value,
			}),
		)?;

		Ok(IERC20::transferCall::abi_encode_returns(&true))
	}

	/// Execute the total supply call.
	fn total_supply(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		use frame_support::traits::fungibles::Inspect;
		env.charge(<Runtime as Config<Instance>>::WeightInfo::total_issuance())?;

		let value =
			Self::to_u256(pallet_assets::Pallet::<Runtime, Instance>::total_issuance(asset_id))?;
		Ok(IERC20::totalSupplyCall::abi_encode_returns(&value))
	}

	/// Execute the balance_of call.
	fn balance_of(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		call: &IERC20::balanceOfCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::balance())?;
		let account = call.account.into_array().into();
		let account = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&account);
		let value =
			Self::to_u256(pallet_assets::Pallet::<Runtime, Instance>::balance(asset_id, account))?;
		Ok(IERC20::balanceOfCall::abi_encode_returns(&value))
	}

	/// Execute the allowance call.
	fn allowance(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		call: &IERC20::allowanceCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::allowance())?;
		use frame_support::traits::fungibles::approvals::Inspect;
		let owner = call.owner.into_array().into();
		let owner = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&owner);

		let spender = call.spender.into_array().into();
		let spender = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&spender);
		let value = Self::to_u256(pallet_assets::Pallet::<Runtime, Instance>::allowance(
			asset_id, &owner, &spender,
		))?;

		Ok(IERC20::allowanceCall::abi_encode_returns(&value))
	}

	/// Execute the approve call.
	///
	/// Implements ERC-20 set semantics: `approve(spender, N)` sets the allowance to exactly `N`
	/// rather than adding to it. When overwriting a non-zero allowance, the existing approval is
	/// cancelled first so the new value replaces (not accumulates with) the old one.
	///
	/// `call.value > Balance::MAX` (the `type(uint256).max` "infinite allowance" idiom)
	/// saturates the stored allowance at `Balance::MAX`. The `Approval` event carries the
	/// raw `call.value`.
	fn approve(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		call: &IERC20::approveCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		use frame_support::traits::fungibles::approvals::Inspect as ApprovalsInspect;

		// Reserve worst-case gas upfront, then refund the unused portion.
		let worst_case = <Runtime as Config<Instance>>::WeightInfo::allowance()
			.saturating_add(<Runtime as Config<Instance>>::WeightInfo::cancel_approval())
			.saturating_add(<Runtime as Config<Instance>>::WeightInfo::approve_transfer());
		let charged = env.charge(worst_case)?;

		let owner = Self::caller(env)?;
		let owner_account =
			<Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&owner);
		let spender: H160 = call.spender.into_array().into();
		let spender_account = env.to_account_id(&spender);
		// Saturate: `type(uint256).max` is the standard "infinite allowance" idiom and must
		// not revert at the conversion boundary.
		let new_amount: <Runtime as Config<Instance>>::Balance = call.value.unique_saturated_into();

		let current = pallet_assets::Pallet::<Runtime, Instance>::allowance(
			asset_id.clone(),
			&owner_account,
			&spender_account,
		);

		let actual_weight;
		if new_amount.is_zero() {
			if !current.is_zero() {
				// Revoke: use the pallet's cancel logic to remove the approval and
				// unreserve the deposit.
				pallet_assets::Pallet::<Runtime, Instance>::do_cancel_approval(
					&asset_id,
					&owner_account,
					&spender_account,
				)?;
				actual_weight = <Runtime as Config<Instance>>::WeightInfo::allowance()
					.saturating_add(<Runtime as Config<Instance>>::WeightInfo::cancel_approval());
			} else {
				// 0→0 no-op: only the allowance read was needed.
				actual_weight = <Runtime as Config<Instance>>::WeightInfo::allowance();
			}
		} else {
			// If there's an existing non-zero allowance, cancel it first so we
			// overwrite (not accumulate) — matching ERC-20 spec semantics.
			// NOTE: This does not mitigate the well-known ERC-20 approve front-running
			// race condition. Callers concerned about this should approve to 0 first,
			// or use increaseAllowance/decreaseAllowance if available.
			if !current.is_zero() {
				pallet_assets::Pallet::<Runtime, Instance>::do_cancel_approval(
					&asset_id,
					&owner_account,
					&spender_account,
				)?;
				actual_weight = worst_case;
			} else {
				actual_weight = <Runtime as Config<Instance>>::WeightInfo::allowance()
					.saturating_add(<Runtime as Config<Instance>>::WeightInfo::approve_transfer());
			}
			pallet_assets::Pallet::<Runtime, Instance>::do_approve_transfer(
				asset_id,
				&owner_account,
				&spender_account,
				new_amount,
			)?;
		}
		env.adjust_gas(charged, actual_weight);

		Self::deposit_event(
			env,
			IERC20Events::Approval(IERC20::Approval {
				owner: owner.0.into(),
				spender: call.spender,
				value: call.value,
			}),
		)?;

		Ok(IERC20::approveCall::abi_encode_returns(&true))
	}

	/// Execute the transfer_from call.
	fn transfer_from(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		call: &IERC20::transferFromCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::transfer_approved())?;
		let spender = Self::caller(env)?;
		let spender = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&spender);

		let from = call.from.into_array().into();
		let from = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&from);

		let to = call.to.into_array().into();
		let to = <Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&to);

		let approval_amount = Self::to_balance(call.value)?;
		pallet_assets::Pallet::<Runtime, Instance>::do_transfer_approved(
			asset_id,
			&from,
			&spender,
			&to,
			approval_amount,
		)?;

		Self::deposit_event(
			env,
			IERC20Events::Transfer(IERC20::Transfer {
				from: call.from,
				to: call.to,
				value: call.value,
			}),
		)?;

		Ok(IERC20::transferFromCall::abi_encode_returns(&true))
	}

	// ==================== ERC20Permit Functions (EIP-2612) ====================

	/// Execute the permit call (EIP-2612).
	///
	/// This verifies the signature, consumes the permit (increments nonce),
	/// and sets the approval. Saturation policy and event payload match `approve` —
	/// see its doc-comment.
	pub(crate) fn permit(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		verifying_contract: H160,
		call: &IERC20::permitCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		// Reserve worst-case gas upfront, then refund the unused portion.
		// The total cost is: use_permit (signature verification + nonce) +
		// worst-case asset approval operations (allowance read + cancel + approve).
		let use_permit_weight = <Runtime as permit::Config>::WeightInfo::use_permit();
		let worst_case = use_permit_weight
			.saturating_add(<Runtime as Config<Instance>>::WeightInfo::allowance())
			.saturating_add(<Runtime as Config<Instance>>::WeightInfo::cancel_approval())
			.saturating_add(<Runtime as Config<Instance>>::WeightInfo::approve_transfer());
		let charged = env.charge(worst_case)?;

		let owner_h160: H160 = call.owner.into_array().into();
		let spender_h160: H160 = call.spender.into_array().into();

		// Convert U256 values to byte arrays
		let value_bytes: [u8; 32] = call.value.to_be_bytes();
		let deadline_bytes: [u8; 32] = call.deadline.to_be_bytes();
		let r_bytes: [u8; 32] = call.r.0;
		let s_bytes: [u8; 32] = call.s.0;

		let transaction_outcome = frame_support::storage::with_transaction(|| {
			let result = (|| {
				// Use the permit - this validates deadline, signature, and increments nonce
				permit::Pallet::<Runtime>::use_permit(
					&verifying_contract,
					&pallet_assets::Pallet::<Runtime, Instance>::name(asset_id.clone()),
					&owner_h160,
					&spender_h160,
					&value_bytes,
					&deadline_bytes,
					call.v,
					&r_bytes,
					&s_bytes,
				)
				.map_err(|e| {
					let msg = match e {
						permit::pallet::Error::PermitExpired => "Permit expired",
						permit::pallet::Error::InvalidSignature => "Invalid signature",
						permit::pallet::Error::SignerMismatch => "Signer does not match owner",
						permit::pallet::Error::SignatureSValueTooHigh => {
							"Signature s value too high (malleability)"
						},
						permit::pallet::Error::InvalidVValue => "Invalid signature v value",
						permit::pallet::Error::NonceOverflow => "Nonce overflow",
						permit::pallet::Error::InvalidOwner => "Invalid owner address",
						permit::pallet::Error::InvalidSpender => "Invalid spender address",
					};
					Error::Revert(Revert { reason: msg.into() })
				})?;

				// Delete-set semantic: cancel any existing approval first so
				// do_approve_transfer sets (not accumulates) the new value.
				use frame_support::traits::fungibles::approvals::Inspect as ApprovalsInspect;
				let owner_account =
					<Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&owner_h160);
				let spender_account =
					<Runtime as pallet_revive::Config>::AddressMapper::to_account_id(&spender_h160);

				// Saturate: see `approve` for the rationale (infinite-allowance idiom).
				let new_amount: <Runtime as Config<Instance>>::Balance =
					call.value.unique_saturated_into();
				let current = pallet_assets::Pallet::<Runtime, Instance>::allowance(
					asset_id.clone(),
					&owner_account,
					&spender_account,
				);

				let actual_weight;
				if new_amount.is_zero() {
					if !current.is_zero() {
						// clear approval if it exists, to match ERC-20 semantics of setting
						// allowance to 0
						pallet_assets::Pallet::<Runtime, Instance>::do_cancel_approval(
							&asset_id,
							&owner_account,
							&spender_account,
						)?;
						actual_weight = use_permit_weight
							.saturating_add(<Runtime as Config<Instance>>::WeightInfo::allowance())
							.saturating_add(
								<Runtime as Config<Instance>>::WeightInfo::cancel_approval(),
							);
					} else {
						// noop: set allowance to zerowhen it is already zero
						actual_weight = use_permit_weight
							.saturating_add(<Runtime as Config<Instance>>::WeightInfo::allowance());
					}
				} else {
					if !current.is_zero() {
						// If there's an existing non-zero allowance, cancel it first
						pallet_assets::Pallet::<Runtime, Instance>::do_cancel_approval(
							&asset_id,
							&owner_account,
							&spender_account,
						)?;
						actual_weight = worst_case;
					} else {
						// set new approval
						actual_weight = use_permit_weight
							.saturating_add(<Runtime as Config<Instance>>::WeightInfo::allowance())
							.saturating_add(
								<Runtime as Config<Instance>>::WeightInfo::approve_transfer(),
							);
					}
					pallet_assets::Pallet::<Runtime, Instance>::do_approve_transfer(
						asset_id,
						&owner_account,
						&spender_account,
						new_amount,
					)?;
				}

				// Emit Approval event
				Self::deposit_event(
					env,
					IERC20Events::Approval(IERC20::Approval {
						owner: call.owner,
						spender: call.spender,
						value: call.value,
					}),
				)?;
				Ok::<_, Error>(actual_weight)
			})();
			match result {
				Ok(actual_weight) => {
					frame_support::storage::TransactionOutcome::Commit(Ok(actual_weight))
				},
				Err(e) => {
					log::trace!(target: frame_support::LOG_TARGET, "Call to permit failed: {e:?}");
					frame_support::storage::TransactionOutcome::Rollback(Err(e))
				},
			}
		});

		// permit returns void
		match transaction_outcome {
			Ok(actual_weight) => {
				env.adjust_gas(charged, actual_weight);
				Ok(Vec::new())
			},
			Err(e) => Err(e),
		}
	}

	/// Get the current nonce for an owner address.
	fn nonces(
		verifying_contract: H160,
		call: &IERC20::noncesCall,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as permit::Config>::WeightInfo::nonces())?;

		let owner_h160: H160 = call.owner.into_array().into();
		let nonce = permit::Pallet::<Runtime>::nonce(&verifying_contract, &owner_h160);

		// Convert sp_core::U256 to alloy U256
		let nonce_bytes = nonce.to_big_endian();
		let nonce_alloy = alloy::primitives::U256::from_be_bytes(nonce_bytes);

		Ok(IERC20::noncesCall::abi_encode_returns(&nonce_alloy))
	}

	/// Get the EIP-712 domain separator for this contract.
	fn domain_separator(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		verifying_contract: H160,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as permit::Config>::WeightInfo::domain_separator())?;

		// Fetch token name for EIP-712 domain separator (per EIP-2612 spec)
		let token_name = pallet_assets::Pallet::<Runtime, Instance>::name(asset_id);

		let separator =
			permit::Pallet::<Runtime>::compute_domain_separator(&verifying_contract, &token_name);
		let separator_alloy: alloy::primitives::FixedBytes<32> = separator.0.into();

		Ok(IERC20::DOMAIN_SEPARATORCall::abi_encode_returns(&separator_alloy))
	}

	/// Execute the name call.
	fn name(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::get_metadata())?;

		let metadata = pallet_assets::Pallet::<Runtime, Instance>::get_metadata(asset_id)
			.ok_or(Error::Revert(Revert { reason: "Metadata not found".into() }))?;

		let name = alloc::string::String::from_utf8(metadata.name.to_vec())
			.map_err(|_| Error::Revert(Revert { reason: "Invalid UTF-8 in name".into() }))?;

		Ok(IERC20::nameCall::abi_encode_returns(&name))
	}

	/// Execute the symbol call.
	fn symbol(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::get_metadata())?;

		let metadata = pallet_assets::Pallet::<Runtime, Instance>::get_metadata(asset_id)
			.ok_or(Error::Revert(Revert { reason: "Metadata not found".into() }))?;

		let symbol = alloc::string::String::from_utf8(metadata.symbol.to_vec())
			.map_err(|_| Error::Revert(Revert { reason: "Invalid UTF-8 in symbol".into() }))?;

		Ok(IERC20::symbolCall::abi_encode_returns(&symbol))
	}

	/// Execute the decimals call.
	fn decimals(
		asset_id: <Runtime as Config<Instance>>::AssetId,
		env: &mut impl Ext<T = Runtime>,
	) -> Result<Vec<u8>, Error> {
		env.charge(<Runtime as Config<Instance>>::WeightInfo::get_metadata())?;

		let metadata = pallet_assets::Pallet::<Runtime, Instance>::get_metadata(asset_id)
			.ok_or(Error::Revert(Revert { reason: "Metadata not found".into() }))?;

		Ok(IERC20::decimalsCall::abi_encode_returns(&metadata.decimals))
	}
}
