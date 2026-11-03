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

//! # Assets Pallet
//!
//! A simple, secure module for dealing with sets of assets implementing
//! [`fungible`](frame_support::traits::fungible) traits, via [`fungibles`] traits.
//!
//! The pallet makes heavy use of concepts such as Holds and Freezes from the
//! [`frame_support::traits::fungible`] traits, therefore you should read and understand those docs
//! as a prerequisite to understanding this pallet.
//!
//! See the [`frame_tokens`] reference docs for more information about the place of the
//! Assets pallet in FRAME.
//!
//! ## Overview
//!
//! The Assets module provides functionality for asset management of fungible asset classes
//! with a fixed supply, including:
//!
//! * Asset Issuance (Minting)
//! * Asset Transferal
//! * Asset Freezing
//! * Asset Destruction (Burning)
//! * Delegated Asset Transfers ("Approval API")
//!
//! To use it in your runtime, you need to implement the assets [`Config`].
//!
//! The supported dispatchable functions are documented in the [`Call`] enum.
//!
//! ### Terminology
//!
//! * **Admin**: An account ID uniquely privileged to be able to unfreeze (thaw) an account and its
//!   assets, as well as forcibly transfer a particular class of assets between arbitrary accounts
//!   and reduce the balance of a particular class of assets of arbitrary accounts.
//! * **Asset issuance/minting**: The creation of a new asset, whose total supply will belong to the
//!   account designated as the beneficiary of the asset. This is a privileged operation.
//! * **Asset transfer**: The reduction of the balance of an asset of one account with the
//!   corresponding increase in the balance of another.
//! * **Asset destruction**: The process of reducing the balance of an asset of one account. This is
//!   a privileged operation.
//! * **Fungible asset**: An asset whose units are interchangeable.
//! * **Issuer**: An account ID uniquely privileged to be able to mint a particular class of assets.
//! * **Freezer**: An account ID uniquely privileged to be able to freeze an account from
//!   transferring a particular class of assets.
//! * **Freezing**: Removing the possibility of an unpermissioned transfer of an asset from a
//!   particular account.
//! * **Non-fungible asset**: An asset for which each unit has unique characteristics.
//! * **Owner**: An account ID uniquely privileged to be able to destroy a particular asset class,
//!   or to set the Issuer, Freezer, Reserves, or Admin of that asset class.
//! * **Approval**: The act of allowing an account the permission to transfer some balance of asset
//!   from the approving account into some third-party destination account.
//! * **Sufficiency**: The idea of a minimum-balance of an asset being sufficient to allow the
//!   account's existence on the system without requiring any other existential-deposit.
//!
//! ### Goals
//!
//! The assets system in Substrate is designed to make the following possible:
//!
//! * Issue new assets in a permissioned or permissionless way, if permissionless, then with a
//!   deposit required.
//! * Allow accounts to be delegated the ability to transfer assets without otherwise existing
//!   on-chain (*approvals*).
//! * Move assets between accounts.
//! * Update an asset class's total supply.
//! * Allow administrative activities by specially privileged accounts including freezing account
//!   balances and minting/burning assets.
//!
//! ## Interface
//!
//! ### Permissionless Functions
//!
//! * `create`: Creates a new asset class, taking the required deposit.
//! * `transfer`: Transfer sender's assets to another account.
//! * `transfer_keep_alive`: Transfer sender's assets to another account, keeping the sender alive.
//! * `approve_transfer`: Create or increase an delegated transfer.
//! * `cancel_approval`: Rescind a previous approval.
//! * `transfer_approved`: Transfer third-party's assets to another account.
//! * `touch`: Create an asset account for non-provider assets. Caller must place a deposit.
//! * `refund`: Return the deposit (if any) of the caller's asset account or a consumer reference
//!   (if any) of the caller's account.
//! * `refund_other`: Return the deposit (if any) of a specified asset account.
//! * `touch_other`: Create an asset account for specified account. Caller must place a deposit.
//!
//! ### Permissioned Functions
//!
//! * `force_create`: Creates a new asset class without taking any deposit.
//! * `force_set_metadata`: Set the metadata of an asset class.
//! * `force_clear_metadata`: Remove the metadata of an asset class.
//! * `force_asset_status`: Alter an asset class's attributes.
//! * `force_cancel_approval`: Rescind a previous approval.
//!
//! ### Privileged Functions
//!
//! * `destroy`: Destroys an entire asset class; called by the asset class's Owner.
//! * `mint`: Increases the asset balance of an account; called by the asset class's Issuer.
//! * `burn`: Decreases the asset balance of an account; called by the asset class's Admin.
//! * `force_transfer`: Transfers between arbitrary accounts; called by the asset class's Admin.
//! * `freeze`: Disallows further `transfer`s from an account; called by the asset class's Freezer.
//! * `thaw`: Allows further `transfer`s to and from an account; called by the asset class's Admin.
//! * `transfer_ownership`: Changes an asset class's Owner; called by the asset class's Owner.
//! * `set_team`: Changes an asset class's Admin, Freezer and Issuer; called by the asset class's
//!   Owner.
//! * `set_metadata`: Set the metadata of an asset class; called by the asset class's Owner.
//! * `clear_metadata`: Remove the metadata of an asset class; called by the asset class's Owner.
//! * `set_reserves`: Set the reserve information of an asset class; called by the asset class's
//!   Owner.
//! * `block`: Disallows further `transfer`s to and from an account; called by the asset class's
//!   Freezer.
//!
//! Please refer to the [`Call`] enum and its associated variants for documentation on each
//! function.
//!
//! ### Public Functions
//! <!-- Original author of descriptions: @gavofyork -->
//!
//! * `balance` - Get the asset `id` balance of `who`.
//! * `total_supply` - Get the total supply of an asset `id`.
//!
//! Please refer to the [`Pallet`] struct for details on publicly available functions.
//!
//! ### Callbacks
//!
//! Using `CallbackHandle` associated type, user can configure custom callback functions which are
//! executed when new asset is created or an existing asset is destroyed.
//!
//! ## Related Modules
//!
//! * [`System`](../frame_system/index.html)
//! * [`Support`](../frame_support/index.html)
//!
//! [`frame_tokens`]: ../polkadot_sdk_docs/reference_docs/frame_tokens/index.html

// This recursion limit is needed because we have too many benchmarks and benchmarking will fail if
// we add more without this limit.
#![recursion_limit = "1024"]
// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;
pub mod migration;
#[cfg(test)]
pub mod mock;
#[cfg(test)]
mod tests;
pub mod weights;

mod extra_mutator;
pub use extra_mutator::*;
mod functions;
mod impl_fungibles;
mod impl_stored_map;
mod types;
pub use types::*;

extern crate alloc;
extern crate core;

use scale_info::TypeInfo;
use sp_runtime::{
	traits::{AtLeast32BitUnsigned, CheckedAdd, CheckedSub, Saturating, StaticLookup, Zero},
	ArithmeticError, DispatchError, TokenError,
};

use alloc::vec::Vec;
use core::{fmt::Debug, marker::PhantomData};
use frame_support::{
	dispatch::DispatchResult,
	ensure,
	pallet_prelude::DispatchResultWithPostInfo,
	storage::KeyPrefixIterator,
	traits::{
		tokens::{
			fungibles, DepositConsequence, Fortitude,
			Preservation::{Expendable, Preserve},
			WithdrawConsequence,
		},
		BalanceStatus::Reserved,
		Currency, EnsureOriginWithArg, Incrementable, ReservableCurrency, StoredMap,
	},
};
use frame_system::Config as SystemConfig;

pub use pallet::*;
pub use weights::WeightInfo;

type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;
const LOG_TARGET: &str = "runtime::assets";

/// Trait with callbacks that are executed after successful asset creation or destruction.
pub trait AssetsCallback<AssetId, AccountId> {
	/// Indicates that asset with `id` was successfully created by the `owner`
	fn created(_id: &AssetId, _owner: &AccountId) -> Result<(), ()> {
		Ok(())
	}

	/// Indicates that asset with `id` has just been destroyed
	fn destroyed(_id: &AssetId) -> Result<(), ()> {
		Ok(())
	}
}

#[impl_trait_for_tuples::impl_for_tuples(10)]
impl<AssetId, AccountId> AssetsCallback<AssetId, AccountId> for Tuple {
	fn created(id: &AssetId, owner: &AccountId) -> Result<(), ()> {
		for_tuples!( #( Tuple::created(id, owner)?; )* );
		Ok(())
	}

	fn destroyed(id: &AssetId) -> Result<(), ()> {
		for_tuples!( #( Tuple::destroyed(id)?; )* );
		Ok(())
	}
}

/// Allocates the asset ids used by `create`.
///
/// Configure [`Config::AssetIdAllocator`] with `()` to leave `create` unconstrained, or with an
/// impl such as [`AutoIncAssetId`] to enforce an allocation policy.
pub trait AssetIdAllocator<AssetId> {
	/// The only id `create` may use, or `None` to accept any id not currently in use.
	fn next() -> Option<AssetId>;

	/// Allocate the id returned by [`Self::next`], after a successful `create`.
	///
	/// Returns `Err` if the id space is exhausted.
	fn advance() -> Result<(), ()>;

	/// Allocate `id`, after a successful `force_create`, so [`Self::next`] can never return it.
	///
	/// Returns `Err` if the id space is exhausted.
	fn advance_from(id: &AssetId) -> Result<(), ()>;
}

/// No allocation policy: `create` accepts any unused id and [`NextAssetId`] has no effect.
impl<AssetId> AssetIdAllocator<AssetId> for () {
	fn next() -> Option<AssetId> {
		None
	}
	fn advance() -> Result<(), ()> {
		Ok(())
	}
	fn advance_from(_: &AssetId) -> Result<(), ()> {
		Ok(())
	}
}

/// Allocates asset ids by auto-incrementing the [`NextAssetId`].
///
/// This has no effect while the [`NextAssetId`] value is not present.
pub struct AutoIncAssetId<T, I = ()>(PhantomData<(T, I)>);
impl<T: Config<I>, I: 'static> AssetIdAllocator<T::AssetId> for AutoIncAssetId<T, I>
where
	T::AssetId: Incrementable + PartialOrd,
{
	fn next() -> Option<T::AssetId> {
		NextAssetId::<T, I>::get()
	}

	fn advance() -> Result<(), ()> {
		let Some(next_id) = NextAssetId::<T, I>::get() else {
			// Auto increment for the asset id is not active.
			return Ok(());
		};
		let next_id = next_id.increment().ok_or(())?;
		NextAssetId::<T, I>::put(next_id);
		Ok(())
	}

	fn advance_from(id: &T::AssetId) -> Result<(), ()> {
		let Some(next_id) = NextAssetId::<T, I>::get() else {
			// Auto increment for the asset id is not active.
			return Ok(());
		};
		// Only advance when the forced id is at or beyond the sequence; ids below `next_id` belong
		// to a range that can be reserved for deliberate, forced assignment.
		if *id >= next_id {
			let next_id = id.increment().ok_or(())?;
			NextAssetId::<T, I>::put(next_id);
		}
		Ok(())
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use codec::HasCompact;
	use frame_support::{
		pallet_prelude::*,
		traits::{tokens::ProvideAssetReserves, AccountTouch, ContainsPair},
	};
	use frame_system::pallet_prelude::*;

	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	/// The maximum number of configurable reserve locations for one asset class.
	pub const MAX_RESERVES: u32 = 5;

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T, I = ()>(_);

	#[cfg(feature = "runtime-benchmarks")]
	pub trait BenchmarkHelper<AssetIdParameter, ReserveIdParameter> {
		fn create_asset_id_parameter(id: u32) -> AssetIdParameter;
		fn create_reserve_id_parameter(id: u32) -> ReserveIdParameter;
	}
	#[cfg(feature = "runtime-benchmarks")]
	impl<AssetIdParameter: From<u32>> BenchmarkHelper<AssetIdParameter, ()> for () {
		fn create_asset_id_parameter(id: u32) -> AssetIdParameter {
			id.into()
		}
		fn create_reserve_id_parameter(_: u32) -> () {
			()
		}
	}

	/// Default implementations of [`DefaultConfig`], which can be used to implement [`Config`].
	pub mod config_preludes {
		use super::*;
		use frame_support::derive_impl;
		pub struct TestDefaultConfig;

		#[derive_impl(frame_system::config_preludes::TestDefaultConfig, no_aggregated_types)]
		impl frame_system::DefaultConfig for TestDefaultConfig {}

		#[frame_support::register_default_impl(TestDefaultConfig)]
		impl DefaultConfig for TestDefaultConfig {
			#[inject_runtime_type]
			type RuntimeEvent = ();
			type Balance = u64;
			type RemoveItemsLimit = ConstU32<5>;
			type AssetId = u32;
			type AssetIdParameter = u32;
			type ReserveData = ();
			type AssetDeposit = ConstUint<1>;
			type AssetAccountDeposit = ConstUint<10>;
			type MetadataDepositBase = ConstUint<1>;
			type MetadataDepositPerByte = ConstUint<1>;
			type ApprovalDeposit = ConstUint<1>;
			type StringLimit = ConstU32<50>;
			type Freezer = ();
			type Holder = ();
			type Extra = ();
			type CallbackHandle = ();
			type AssetIdAllocator = ();
			type WeightInfo = ();
			#[cfg(feature = "runtime-benchmarks")]
			type BenchmarkHelper = ();
		}
	}

	#[pallet::config(with_default)]
	/// The module configuration trait.
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The overarching event type.
		#[pallet::no_default_bounds]
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The units in which we record balances.
		type Balance: Member
			+ Parameter
			+ HasCompact<Type: DecodeWithMemTracking>
			+ AtLeast32BitUnsigned
			+ Default
			+ Copy
			+ MaybeSerializeDeserialize
			+ MaxEncodedLen
			+ TypeInfo;

		/// Max number of items to destroy per `destroy_accounts` and `destroy_approvals` call.
		///
		/// Must be configured to result in a weight that makes each call fit in a block.
		#[pallet::constant]
		type RemoveItemsLimit: Get<u32>;

		/// Identifier for the class of asset.
		type AssetId: Member + Parameter + Clone + MaybeSerializeDeserialize + MaxEncodedLen;

		/// Wrapper around `Self::AssetId` to use in dispatchable call signatures. Allows the use
		/// of compact encoding in instances of the pallet, which will prevent breaking changes
		/// resulting from the removal of `HasCompact` from `Self::AssetId`.
		///
		/// This type includes the `From<Self::AssetId>` bound, since tightly coupled pallets may
		/// want to convert an `AssetId` into a parameter for calling dispatchable functions
		/// directly.
		type AssetIdParameter: Parameter + From<Self::AssetId> + Into<Self::AssetId> + MaxEncodedLen;

		/// Information about reserve locations for a class of asset.
		type ReserveData: Debug + Parameter + MaybeSerializeDeserialize + MaxEncodedLen;

		/// The currency mechanism.
		#[pallet::no_default]
		type Currency: ReservableCurrency<Self::AccountId>;

		/// Standard asset class creation is only allowed if the origin attempting it and the
		/// asset class are in this set.
		#[pallet::no_default]
		type CreateOrigin: EnsureOriginWithArg<
			Self::RuntimeOrigin,
			Self::AssetId,
			Success = Self::AccountId,
		>;

		/// The origin which may forcibly create or destroy an asset or otherwise alter privileged
		/// attributes.
		#[pallet::no_default]
		type ForceOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The basic amount of funds that must be reserved for an asset.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type AssetDeposit: Get<DepositBalanceOf<Self, I>>;

		/// The amount of funds that must be reserved for a non-provider asset account to be
		/// maintained.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type AssetAccountDeposit: Get<DepositBalanceOf<Self, I>>;

		/// The basic amount of funds that must be reserved when adding metadata to your asset.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type MetadataDepositBase: Get<DepositBalanceOf<Self, I>>;

		/// The additional funds that must be reserved for the number of bytes you store in your
		/// metadata.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type MetadataDepositPerByte: Get<DepositBalanceOf<Self, I>>;

		/// The amount of funds that must be reserved when creating a new approval.
		#[pallet::constant]
		#[pallet::no_default_bounds]
		type ApprovalDeposit: Get<DepositBalanceOf<Self, I>>;

		/// The maximum length of a name or symbol stored on-chain.
		#[pallet::constant]
		type StringLimit: Get<u32>;

		/// A hook to allow a per-asset, per-account minimum balance to be enforced. This must be
		/// respected in all permissionless operations.
		type Freezer: FrozenBalance<Self::AssetId, Self::AccountId, Self::Balance>;

		/// A hook to inspect a per-asset, per-account balance that is held. This goes in
		/// accordance with balance model.
		type Holder: BalanceOnHold<Self::AssetId, Self::AccountId, Self::Balance>;

		/// Additional data to be stored with an account's asset balance.
		type Extra: Member + Parameter + Default + MaxEncodedLen;

		/// Callback methods for asset state change (e.g. asset created or destroyed)
		///
		/// Types implementing the [`AssetsCallback`] can be chained when listed together as a
		/// tuple.
		///
		/// Do NOT allocate asset ids through this; use [`Config::AssetIdAllocator`]. A callback
		/// that also mutates [`NextAssetId`] desyncs [`AutoIncAssetId`].
		type CallbackHandle: AssetsCallback<Self::AssetId, Self::AccountId>;

		/// Allocates the asset ids used by `create`. See [`AssetIdAllocator`].
		///
		/// Set to `()` to leave `create` unconstrained, or to [`AutoIncAssetId`] (with an
		/// initialized [`NextAssetId`]) for auto-incrementing ids.
		type AssetIdAllocator: AssetIdAllocator<Self::AssetId>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// Helper trait for benchmarks.
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: BenchmarkHelper<Self::AssetIdParameter, Self::ReserveData>;
	}

	#[pallet::storage]
	/// Details of an asset.
	pub type Asset<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		AssetDetails<T::Balance, T::AccountId, DepositBalanceOf<T, I>>,
	>;

	#[pallet::storage]
	/// The holdings of a specific account for a specific asset.
	pub type Account<T: Config<I>, I: 'static = ()> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		Blake2_128Concat,
		T::AccountId,
		AssetAccountOf<T, I>,
	>;

	#[pallet::storage]
	/// Approved balance transfers. First balance is the amount approved for transfer. Second
	/// is the amount of `T::Currency` reserved for storing this.
	/// First key is the asset ID, second key is the owner and third key is the delegate.
	pub type Approvals<T: Config<I>, I: 'static = ()> = StorageNMap<
		_,
		(
			NMapKey<Blake2_128Concat, T::AssetId>,
			NMapKey<Blake2_128Concat, T::AccountId>, // owner
			NMapKey<Blake2_128Concat, T::AccountId>, // delegate
		),
		Approval<T::Balance, DepositBalanceOf<T, I>>,
	>;

	#[pallet::storage]
	/// Metadata of an asset.
	pub type Metadata<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		AssetMetadata<DepositBalanceOf<T, I>, BoundedVec<u8, T::StringLimit>>,
		ValueQuery,
	>;

	/// Maps an asset to a list of its configured reserve information.
	#[pallet::storage]
	pub type Reserves<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		BoundedVec<T::ReserveData, ConstU32<MAX_RESERVES>>,
		ValueQuery,
	>;

	/// The asset ID enforced for the next asset creation, if any present. Otherwise, this storage
	/// item has no effect.
	///
	/// Only read by [`crate::AutoIncAssetId`]: configure it as [`Config::AssetIdAllocator`] and
	/// provide an initial value here to apply an auto-increment model to all new asset IDs.
	///
	/// The initial next asset ID can be set using the [`GenesisConfig`] or the
	/// [SetNextAssetId](`migration::next_asset_id::SetNextAssetId`) migration.
	#[pallet::storage]
	pub type NextAssetId<T: Config<I>, I: 'static = ()> = StorageValue<_, T::AssetId, OptionQuery>;

	#[pallet::genesis_config]
	#[derive(frame_support::DefaultNoBound)]
	pub struct GenesisConfig<T: Config<I>, I: 'static = ()> {
		/// Genesis assets: id, owner, is_sufficient, min_balance
		pub assets: Vec<(T::AssetId, T::AccountId, bool, T::Balance)>,
		/// Genesis metadata: id, name, symbol, decimals
		pub metadata: Vec<(T::AssetId, Vec<u8>, Vec<u8>, u8)>,
		/// Genesis accounts: id, account_id, balance
		pub accounts: Vec<(T::AssetId, T::AccountId, T::Balance)>,
		/// Genesis [`NextAssetId`].
		///
		/// Refer to the [`NextAssetId`] item for more information.
		///
		/// This does not enforce the asset ID for the [assets](`GenesisConfig::assets`) within the
		/// genesis config. It sets the [`NextAssetId`] after they have been created.
		pub next_asset_id: Option<T::AssetId>,
		/// Genesis assets and their reserves
		pub reserves: Vec<(T::AssetId, Vec<T::ReserveData>)>,
	}

	#[pallet::genesis_build]
	impl<T: Config<I>, I: 'static> BuildGenesisConfig for GenesisConfig<T, I> {
		fn build(&self) {
			for (id, owner, is_sufficient, min_balance) in &self.assets {
				assert!(!Asset::<T, I>::contains_key(id), "Asset id already in use");
				assert!(!min_balance.is_zero(), "Min balance should not be zero");
				Asset::<T, I>::insert(
					id,
					AssetDetails {
						owner: owner.clone(),
						issuer: owner.clone(),
						admin: owner.clone(),
						freezer: owner.clone(),
						supply: Zero::zero(),
						deposit: Zero::zero(),
						min_balance: *min_balance,
						is_sufficient: *is_sufficient,
						accounts: 0,
						sufficients: 0,
						approvals: 0,
						status: AssetStatus::Live,
					},
				);
			}

			for (id, name, symbol, decimals) in &self.metadata {
				assert!(Asset::<T, I>::contains_key(id), "Asset does not exist");

				let bounded_name: BoundedVec<u8, T::StringLimit> =
					name.clone().try_into().expect("asset name is too long");
				let bounded_symbol: BoundedVec<u8, T::StringLimit> =
					symbol.clone().try_into().expect("asset symbol is too long");

				let metadata = AssetMetadata {
					deposit: Zero::zero(),
					name: bounded_name,
					symbol: bounded_symbol,
					decimals: *decimals,
					is_frozen: false,
				};
				Metadata::<T, I>::insert(id, metadata);
			}

			for (id, account_id, amount) in &self.accounts {
				let result = <Pallet<T, I>>::increase_balance(
					id.clone(),
					account_id,
					*amount,
					|details| -> DispatchResult {
						debug_assert!(
							details.supply.checked_add(&amount).is_some(),
							"checked in prep; qed"
						);
						details.supply = details.supply.saturating_add(*amount);
						Ok(())
					},
				);
				assert!(result.is_ok());
			}

			if let Some(next_asset_id) = &self.next_asset_id {
				NextAssetId::<T, I>::put(next_asset_id);
			}

			for (id, reserves) in &self.reserves {
				assert!(!Reserves::<T, I>::contains_key(id), "Asset id already in use");
				let reserves = BoundedVec::try_from(reserves.clone()).expect("too many reserves");
				Reserves::<T, I>::insert(id, reserves);
			}
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// Some asset class was created.
		Created { asset_id: T::AssetId, creator: T::AccountId, owner: T::AccountId },
		/// Some assets were issued.
		Issued { asset_id: T::AssetId, owner: T::AccountId, amount: T::Balance },
		/// Some assets were transferred.
		Transferred {
			asset_id: T::AssetId,
			from: T::AccountId,
			to: T::AccountId,
			amount: T::Balance,
		},
		/// Some assets were destroyed.
		Burned { asset_id: T::AssetId, owner: T::AccountId, balance: T::Balance },
		/// The management team changed.
		TeamChanged {
			asset_id: T::AssetId,
			issuer: T::AccountId,
			admin: T::AccountId,
			freezer: T::AccountId,
		},
		/// The owner changed.
		OwnerChanged { asset_id: T::AssetId, owner: T::AccountId },
		/// Some account `who` was frozen.
		Frozen { asset_id: T::AssetId, who: T::AccountId },
		/// Some account `who` was thawed.
		Thawed { asset_id: T::AssetId, who: T::AccountId },
		/// Some asset `asset_id` was frozen.
		AssetFrozen { asset_id: T::AssetId },
		/// Some asset `asset_id` was thawed.
		AssetThawed { asset_id: T::AssetId },
		/// Accounts were destroyed for given asset.
		AccountsDestroyed { asset_id: T::AssetId, accounts_destroyed: u32, accounts_remaining: u32 },
		/// Approvals were destroyed for given asset.
		ApprovalsDestroyed {
			asset_id: T::AssetId,
			approvals_destroyed: u32,
			approvals_remaining: u32,
		},
		/// An asset class is in the process of being destroyed.
		DestructionStarted { asset_id: T::AssetId },
		/// An asset class was destroyed.
		Destroyed { asset_id: T::AssetId },
		/// Some asset class was force-created.
		ForceCreated { asset_id: T::AssetId, owner: T::AccountId },
		/// New metadata has been set for an asset.
		MetadataSet {
			asset_id: T::AssetId,
			name: Vec<u8>,
			symbol: Vec<u8>,
			decimals: u8,
			is_frozen: bool,
		},
		/// Metadata has been cleared for an asset.
		MetadataCleared { asset_id: T::AssetId },
		/// (Additional) funds have been approved for transfer to a destination account.
		ApprovedTransfer {
			asset_id: T::AssetId,
			source: T::AccountId,
			delegate: T::AccountId,
			amount: T::Balance,
		},
		/// An approval for account `delegate` was cancelled by `owner`.
		ApprovalCancelled { asset_id: T::AssetId, owner: T::AccountId, delegate: T::AccountId },
		/// An `amount` was transferred in its entirety from `owner` to `destination` by
		/// the approved `delegate`.
		TransferredApproved {
			asset_id: T::AssetId,
			owner: T::AccountId,
			delegate: T::AccountId,
			destination: T::AccountId,
			amount: T::Balance,
		},
		/// An asset has had its attributes changed by the `Force` origin.
		AssetStatusChanged { asset_id: T::AssetId },
		/// The min_balance of an asset has been updated by the asset owner.
		AssetMinBalanceChanged { asset_id: T::AssetId, new_min_balance: T::Balance },
		/// Some account `who` was created with a deposit from `depositor`.
		Touched { asset_id: T::AssetId, who: T::AccountId, depositor: T::AccountId },
		/// Some account `who` was blocked.
		Blocked { asset_id: T::AssetId, who: T::AccountId },
		/// Some assets were deposited (e.g. for transaction fees).
		Deposited { asset_id: T::AssetId, who: T::AccountId, amount: T::Balance },
		/// Some assets were withdrawn from the account (e.g. for transaction fees).
		Withdrawn { asset_id: T::AssetId, who: T::AccountId, amount: T::Balance },
		/// Reserve information was set or updated for `asset_id`.
		ReservesUpdated { asset_id: T::AssetId, reserves: Vec<T::ReserveData> },
		/// Reserve information was removed for `asset_id`.
		ReservesRemoved { asset_id: T::AssetId },
		/// Some assets were issued as Credit (no owner yet).
		IssuedCredit { asset_id: T::AssetId, amount: T::Balance },
		/// Some assets Credit was destroyed.
		BurnedCredit { asset_id: T::AssetId, amount: T::Balance },
		/// Some assets were burned and a Debt was created.
		IssuedDebt { asset_id: T::AssetId, amount: T::Balance },
		/// Some assets Debt was destroyed (and assets issued).
		BurnedDebt { asset_id: T::AssetId, amount: T::Balance },
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Account balance must be greater than or equal to the transfer amount.
		BalanceLow,
		/// The account to alter does not exist.
		NoAccount,
		/// The signing account has no permission to do the operation.
		NoPermission,
		/// The given asset ID is unknown.
		Unknown,
		/// The origin account is frozen.
		Frozen,
		/// The asset ID is already taken.
		InUse,
		/// Invalid witness data given.
		BadWitness,
		/// Minimum balance should be non-zero.
		MinBalanceZero,
		/// Unable to increment the consumer reference counters on the account. Either no provider
		/// reference exists to allow a non-zero balance of a non-self-sufficient asset, or one
		/// fewer then the maximum number of consumers has been reached.
		UnavailableConsumer,
		/// Invalid metadata given.
		BadMetadata,
		/// No approval exists that would allow the transfer.
		Unapproved,
		/// The source account would not survive the transfer and it needs to stay alive.
		WouldDie,
		/// The asset-account already exists.
		AlreadyExists,
		/// The asset-account doesn't have an associated deposit.
		NoDeposit,
		/// The operation would result in funds being burned.
		WouldBurn,
		/// The asset is a live asset and is actively being used. Usually emit for operations such
		/// as `start_destroy` which require the asset to be in a destroying state.
		LiveAsset,
		/// The asset is not live, and likely being destroyed.
		AssetNotLive,
		/// The asset status is not the expected status.
		IncorrectStatus,
		/// The asset should be frozen before the given operation.
		NotFrozen,
		/// Callback action resulted in error
		CallbackFailed,
		/// The asset ID is not the one required by [`Config::AssetIdAllocator`].
		BadAssetId,
		/// The [`Config::AssetIdAllocator`] cannot allocate the asset ID: the id space is
		/// exhausted.
		AssetIdAllocationFailed,
		/// The asset cannot be destroyed because some accounts for this asset contain freezes.
		ContainsFreezes,
		/// The asset cannot be destroyed because some accounts for this asset contain holds.
		ContainsHolds,
		/// Tried setting too many reserves.
		TooManyReserves,
		/// The asset deposit could not be fully moved due to a lock or freeze on the owner.
		IncompleteDepositTransfer,
	}

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}

	#[pallet::call(weight(<T as Config<I>>::WeightInfo))]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Issue a new class of fungible assets from a public origin.
		///
		/// This new asset class has no assets initially and its owner is the origin.
		///
		/// The origin must conform to the configured `CreateOrigin` and have sufficient funds free.
		///
		/// Funds of sender are reserved by `AssetDeposit`.
		///
		/// Parameters:
		/// - `id`: The identifier of the new asset. This must not be currently in use to identify
		/// an existing asset. If [`Config::AssetIdAllocator`] requires a specific id, this must
		/// equal it.
		/// - `admin`: The admin of this class of assets. The admin is the initial address of each
		/// member of the asset class's admin team.
		/// - `min_balance`: The minimum balance of this new asset that any single account must
		/// have. If an account's balance is reduced below this, then it collapses to zero.
		///
		/// Emits `Created` event when successful.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(0)]
		pub fn create(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			admin: AccountIdLookupOf<T>,
			min_balance: T::Balance,
		) -> DispatchResult {
			let id: T::AssetId = id.into();
			let owner = T::CreateOrigin::ensure_origin(origin, &id)?;
			let admin = T::Lookup::lookup(admin)?;

			ensure!(!Asset::<T, I>::contains_key(&id), Error::<T, I>::InUse);
			ensure!(!min_balance.is_zero(), Error::<T, I>::MinBalanceZero);

			if let Some(next_id) = T::AssetIdAllocator::next() {
				ensure!(id == next_id, Error::<T, I>::BadAssetId);
			}

			let deposit = T::AssetDeposit::get();
			T::Currency::reserve(&owner, deposit)?;

			Asset::<T, I>::insert(
				id.clone(),
				AssetDetails {
					owner: owner.clone(),
					issuer: admin.clone(),
					admin: admin.clone(),
					freezer: admin.clone(),
					supply: Zero::zero(),
					deposit,
					min_balance,
					is_sufficient: false,
					accounts: 0,
					sufficients: 0,
					approvals: 0,
					status: AssetStatus::Live,
				},
			);
			ensure!(T::CallbackHandle::created(&id, &owner).is_ok(), Error::<T, I>::CallbackFailed);
			T::AssetIdAllocator::advance().map_err(|_| Error::<T, I>::AssetIdAllocationFailed)?;
			Self::deposit_event(Event::Created {
				asset_id: id,
				creator: owner.clone(),
				owner: admin,
			});

			Ok(())
		}

		/// Issue a new class of fungible assets from a privileged origin.
		///
		/// This new asset class has no assets initially.
		///
		/// The origin must conform to `ForceOrigin`.
		///
		/// Unlike `create`, no funds are reserved.
		///
		/// Unlike `create`, the `id` does not have to be the one required by
		/// [`Config::AssetIdAllocator`]: a privileged origin may pick any `id`, which is then
		/// marked as allocated.
		///
		/// # Warning
		///
		/// Forcing an arbitrary `id` is dangerous: the pallet only checks that `id` is not
		/// *currently* in use, not that it was never used before. Reusing an id can corrupt state,
		/// most severely for bridged assets, where a collision breaks the local/remote mapping.
		///
		/// - `id`: The identifier of the new asset. This must not be currently in use to identify
		/// an existing asset, and must never have been in use previously (see warning above).
		/// - `owner`: The owner of this class of assets. The owner has full superuser permissions
		/// over this asset, but may later change and configure the permissions using
		/// `transfer_ownership` and `set_team`.
		/// - `min_balance`: The minimum balance of this new asset that any single account must
		/// have. If an account's balance is reduced below this, then it collapses to zero.
		///
		/// Emits `ForceCreated` event when successful.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(1)]
		pub fn force_create(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			owner: AccountIdLookupOf<T>,
			is_sufficient: bool,
			#[pallet::compact] min_balance: T::Balance,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			let owner = T::Lookup::lookup(owner)?;
			let id: T::AssetId = id.into();
			Self::do_force_create(id, owner, is_sufficient, min_balance, false)
		}

		/// Start the process of destroying a fungible asset class.
		///
		/// `start_destroy` is the first in a series of extrinsics that should be called, to allow
		/// destruction of an asset class.
		///
		/// The origin must conform to `ForceOrigin` or must be `Signed` by the asset's `owner`.
		///
		/// - `id`: The identifier of the asset to be destroyed. This must identify an existing
		///   asset.
		///
		/// It will fail with either [`Error::ContainsHolds`] or [`Error::ContainsFreezes`] if
		/// an account contains holds or freezes in place.
		#[pallet::call_index(2)]
		pub fn start_destroy(origin: OriginFor<T>, id: T::AssetIdParameter) -> DispatchResult {
			let maybe_check_owner = match T::ForceOrigin::try_origin(origin) {
				Ok(_) => None,
				Err(origin) => Some(ensure_signed(origin)?),
			};
			let id: T::AssetId = id.into();
			Self::do_start_destroy(id, maybe_check_owner)
		}

		/// Destroy all accounts associated with a given asset.
		///
		/// `destroy_accounts` should only be called after `start_destroy` has been called, and the
		/// asset is in a `Destroying` state.
		///
		/// Due to weight restrictions, this function may need to be called multiple times to fully
		/// destroy all accounts. It will destroy `RemoveItemsLimit` accounts at a time.
		///
		/// - `id`: The identifier of the asset to be destroyed. This must identify an existing
		///   asset.
		///
		/// Each call emits the `Event::DestroyedAccounts` event.
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::destroy_accounts(T::RemoveItemsLimit::get()))]
		pub fn destroy_accounts(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			let id: T::AssetId = id.into();
			let removed_accounts = Self::do_destroy_accounts(id, T::RemoveItemsLimit::get())?;
			Ok(Some(T::WeightInfo::destroy_accounts(removed_accounts)).into())
		}

		/// Destroy all approvals associated with a given asset up to the max (T::RemoveItemsLimit).
		///
		/// `destroy_approvals` should only be called after `start_destroy` has been called, and the
		/// asset is in a `Destroying` state.
		///
		/// Due to weight restrictions, this function may need to be called multiple times to fully
		/// destroy all approvals. It will destroy `RemoveItemsLimit` approvals at a time.
		///
		/// - `id`: The identifier of the asset to be destroyed. This must identify an existing
		///   asset.
		///
		/// Each call emits the `Event::DestroyedApprovals` event.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::destroy_approvals(T::RemoveItemsLimit::get()))]
		pub fn destroy_approvals(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			let id: T::AssetId = id.into();
			let removed_approvals = Self::do_destroy_approvals(id, T::RemoveItemsLimit::get())?;
			Ok(Some(T::WeightInfo::destroy_approvals(removed_approvals)).into())
		}

		/// Complete destroying asset and unreserve currency.
		///
		/// `finish_destroy` should only be called after `start_destroy` has been called, and the
		/// asset is in a `Destroying` state. All accounts or approvals should be destroyed before
		/// hand.
		///
		/// - `id`: The identifier of the asset to be destroyed. This must identify an existing
		///   asset.
		///
		/// Each successful call emits the `Event::Destroyed` event.
		#[pallet::call_index(5)]
		pub fn finish_destroy(origin: OriginFor<T>, id: T::AssetIdParameter) -> DispatchResult {
			ensure_signed(origin)?;
			let id: T::AssetId = id.into();
			Self::do_finish_destroy(id)
		}

		/// Mint assets of a particular class.
		///
		/// The origin must be Signed and the sender must be the Issuer of the asset `id`.
		///
		/// - `id`: The identifier of the asset to have some amount minted.
		/// - `beneficiary`: The account to be credited with the minted assets.
		/// - `amount`: The amount of the asset to be minted.
		///
		/// Emits `Issued` event when successful.
		///
		/// Weight: `O(1)`
		/// Modes: Pre-existing balance of `beneficiary`; Account pre-existence of `beneficiary`.
		#[pallet::call_index(6)]
		pub fn mint(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			beneficiary: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let beneficiary = T::Lookup::lookup(beneficiary)?;
			let id: T::AssetId = id.into();
			Self::do_mint(id, &beneficiary, amount, Some(origin))?;
			Ok(())
		}

		/// Reduce the balance of `who` by as much as possible up to `amount` assets of `id`.
		///
		/// Origin must be Signed and the sender should be the Manager of the asset `id`.
		///
		/// Bails with `NoAccount` if the `who` is already dead.
		///
		/// - `id`: The identifier of the asset to have some amount burned.
		/// - `who`: The account to be debited from.
		/// - `amount`: The maximum amount by which `who`'s balance should be reduced.
		///
		/// Emits `Burned` with the actual amount burned. If this takes the balance to below the
		/// minimum for the asset, then the amount burned is increased to take it to zero.
		///
		/// Weight: `O(1)`
		/// Modes: Post-existence of `who`; Pre & post Zombie-status of `who`.
		#[pallet::call_index(7)]
		pub fn burn(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			who: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let who = T::Lookup::lookup(who)?;
			let id: T::AssetId = id.into();

			let f = DebitFlags { keep_alive: false, best_effort: true };
			Self::do_burn(id, &who, amount, Some(origin), f)?;
			Ok(())
		}

		/// Move some assets from the sender account to another.
		///
		/// Origin must be Signed.
		///
		/// - `id`: The identifier of the asset to have some amount transferred.
		/// - `target`: The account to be credited.
		/// - `amount`: The amount by which the sender's balance of assets should be reduced and
		/// `target`'s balance increased. The amount actually transferred may be slightly greater in
		/// the case that the transfer would otherwise take the sender balance above zero but below
		/// the minimum balance. Must be greater than zero.
		///
		/// Emits `Transferred` with the actual amount transferred. If this takes the source balance
		/// to below the minimum for the asset, then the amount transferred is increased to take it
		/// to zero.
		///
		/// Weight: `O(1)`
		/// Modes: Pre-existence of `target`; Post-existence of sender; Account pre-existence of
		/// `target`.
		#[pallet::call_index(8)]
		pub fn transfer(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			target: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(target)?;
			let id: T::AssetId = id.into();

			let f = TransferFlags { keep_alive: false, best_effort: false, burn_dust: false };
			Self::do_transfer(id, &origin, &dest, amount, None, f).map(|_| ())
		}

		/// Move some assets from the sender account to another, keeping the sender account alive.
		///
		/// Origin must be Signed.
		///
		/// - `id`: The identifier of the asset to have some amount transferred.
		/// - `target`: The account to be credited.
		/// - `amount`: The amount by which the sender's balance of assets should be reduced and
		/// `target`'s balance increased. The amount actually transferred may be slightly greater in
		/// the case that the transfer would otherwise take the sender balance above zero but below
		/// the minimum balance. Must be greater than zero.
		///
		/// Emits `Transferred` with the actual amount transferred. If this takes the source balance
		/// to below the minimum for the asset, then the amount transferred is increased to take it
		/// to zero.
		///
		/// Weight: `O(1)`
		/// Modes: Pre-existence of `target`; Post-existence of sender; Account pre-existence of
		/// `target`.
		#[pallet::call_index(9)]
		pub fn transfer_keep_alive(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			target: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let source = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(target)?;
			let id: T::AssetId = id.into();

			let f = TransferFlags { keep_alive: true, best_effort: false, burn_dust: false };
			Self::do_transfer(id, &source, &dest, amount, None, f).map(|_| ())
		}

		/// Move some assets from one account to another.
		///
		/// Origin must be Signed and the sender should be the Admin of the asset `id`.
		///
		/// - `id`: The identifier of the asset to have some amount transferred.
		/// - `source`: The account to be debited.
		/// - `dest`: The account to be credited.
		/// - `amount`: The amount by which the `source`'s balance of assets should be reduced and
		/// `dest`'s balance increased. The amount actually transferred may be slightly greater in
		/// the case that the transfer would otherwise take the `source` balance above zero but
		/// below the minimum balance. Must be greater than zero.
		///
		/// Emits `Transferred` with the actual amount transferred. If this takes the source balance
		/// to below the minimum for the asset, then the amount transferred is increased to take it
		/// to zero.
		///
		/// Weight: `O(1)`
		/// Modes: Pre-existence of `dest`; Post-existence of `source`; Account pre-existence of
		/// `dest`.
		#[pallet::call_index(10)]
		pub fn force_transfer(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			source: AccountIdLookupOf<T>,
			dest: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let source = T::Lookup::lookup(source)?;
			let dest = T::Lookup::lookup(dest)?;
			let id: T::AssetId = id.into();

			let f = TransferFlags { keep_alive: false, best_effort: false, burn_dust: false };
			Self::do_transfer(id, &source, &dest, amount, Some(origin), f).map(|_| ())
		}

		/// Disallow further unprivileged transfers of an asset `id` from an account `who`. `who`
		/// must already exist as an entry in `Account`s of the asset. If you want to freeze an
		/// account that does not have an entry, use `touch_other` first.
		///
		/// Origin must be Signed and the sender should be the Freezer of the asset `id`.
		///
		/// - `id`: The identifier of the asset to be frozen.
		/// - `who`: The account to be frozen.
		///
		/// Emits `Frozen`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(11)]
		pub fn freeze(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			who: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			let d = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			ensure!(
				d.status == AssetStatus::Live || d.status == AssetStatus::Frozen,
				Error::<T, I>::IncorrectStatus
			);
			ensure!(origin == d.freezer, Error::<T, I>::NoPermission);
			let who = T::Lookup::lookup(who)?;

			Account::<T, I>::try_mutate(&id, &who, |maybe_account| -> DispatchResult {
				maybe_account.as_mut().ok_or(Error::<T, I>::NoAccount)?.status =
					AccountStatus::Frozen;
				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::Frozen { asset_id: id, who });
			Ok(())
		}

		/// Allow unprivileged transfers to and from an account again.
		///
		/// Origin must be Signed and the sender should be the Admin of the asset `id`.
		///
		/// - `id`: The identifier of the asset to be frozen.
		/// - `who`: The account to be unfrozen.
		///
		/// Emits `Thawed`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(12)]
		pub fn thaw(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			who: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			let details = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			ensure!(
				details.status == AssetStatus::Live || details.status == AssetStatus::Frozen,
				Error::<T, I>::IncorrectStatus
			);
			ensure!(origin == details.admin, Error::<T, I>::NoPermission);
			let who = T::Lookup::lookup(who)?;

			Account::<T, I>::try_mutate(&id, &who, |maybe_account| -> DispatchResult {
				maybe_account.as_mut().ok_or(Error::<T, I>::NoAccount)?.status =
					AccountStatus::Liquid;
				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::Thawed { asset_id: id, who });
			Ok(())
		}

		/// Disallow further unprivileged transfers for the asset class.
		///
		/// Origin must be Signed and the sender should be the Freezer of the asset `id`.
		///
		/// - `id`: The identifier of the asset to be frozen.
		///
		/// Emits `Frozen`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(13)]
		pub fn freeze_asset(origin: OriginFor<T>, id: T::AssetIdParameter) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			Asset::<T, I>::try_mutate(id.clone(), |maybe_details| {
				let d = maybe_details.as_mut().ok_or(Error::<T, I>::Unknown)?;
				ensure!(d.status == AssetStatus::Live, Error::<T, I>::AssetNotLive);
				ensure!(origin == d.freezer, Error::<T, I>::NoPermission);

				d.status = AssetStatus::Frozen;

				Self::deposit_event(Event::<T, I>::AssetFrozen { asset_id: id });
				Ok(())
			})
		}

		/// Allow unprivileged transfers for the asset again.
		///
		/// Origin must be Signed and the sender should be the Admin of the asset `id`.
		///
		/// - `id`: The identifier of the asset to be thawed.
		///
		/// Emits `Thawed`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(14)]
		pub fn thaw_asset(origin: OriginFor<T>, id: T::AssetIdParameter) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			Asset::<T, I>::try_mutate(id.clone(), |maybe_details| {
				let d = maybe_details.as_mut().ok_or(Error::<T, I>::Unknown)?;
				ensure!(origin == d.admin, Error::<T, I>::NoPermission);
				ensure!(d.status == AssetStatus::Frozen, Error::<T, I>::NotFrozen);

				d.status = AssetStatus::Live;

				Self::deposit_event(Event::<T, I>::AssetThawed { asset_id: id });
				Ok(())
			})
		}

		/// Change the Owner of an asset.
		///
		/// Origin must be Signed and the sender should be the Owner of the asset `id`.
		///
		/// The asset (and metadata) deposit is moved from the current to the new owner. Fails
		/// with [`Error::IncompleteDepositTransfer`] if a lock or freeze on the current owner
		/// blocks the full transfer; clear it and retry.
		///
		/// - `id`: The identifier of the asset.
		/// - `owner`: The new Owner of this asset.
		///
		/// Emits `OwnerChanged`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(15)]
		pub fn transfer_ownership(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			owner: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let owner = T::Lookup::lookup(owner)?;
			let id: T::AssetId = id.into();

			Asset::<T, I>::try_mutate(id.clone(), |maybe_details| {
				let details = maybe_details.as_mut().ok_or(Error::<T, I>::Unknown)?;
				ensure!(details.status == AssetStatus::Live, Error::<T, I>::AssetNotLive);
				ensure!(origin == details.owner, Error::<T, I>::NoPermission);
				if details.owner == owner {
					return Ok(());
				}

				let metadata_deposit = Metadata::<T, I>::get(&id).deposit;
				let deposit = details.deposit + metadata_deposit;

				// `repatriate_reserved` is best-effort: reject any partial move so the recorded
				// deposit stays in sync with what is actually reserved on the owner.
				let remaining =
					T::Currency::repatriate_reserved(&details.owner, &owner, deposit, Reserved)?;
				ensure!(remaining.is_zero(), Error::<T, I>::IncompleteDepositTransfer);

				details.owner = owner.clone();

				Self::deposit_event(Event::OwnerChanged { asset_id: id, owner });
				Ok(())
			})
		}

		/// Change the Issuer, Admin and Freezer of an asset.
		///
		/// Origin must be Signed and the sender should be the Owner of the asset `id`.
		///
		/// - `id`: The identifier of the asset to be frozen.
		/// - `issuer`: The new Issuer of this asset.
		/// - `admin`: The new Admin of this asset.
		/// - `freezer`: The new Freezer of this asset.
		///
		/// Emits `TeamChanged`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(16)]
		pub fn set_team(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			issuer: AccountIdLookupOf<T>,
			admin: AccountIdLookupOf<T>,
			freezer: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let issuer = T::Lookup::lookup(issuer)?;
			let admin = T::Lookup::lookup(admin)?;
			let freezer = T::Lookup::lookup(freezer)?;
			let id: T::AssetId = id.into();

			Asset::<T, I>::try_mutate(id.clone(), |maybe_details| {
				let details = maybe_details.as_mut().ok_or(Error::<T, I>::Unknown)?;
				ensure!(details.status == AssetStatus::Live, Error::<T, I>::AssetNotLive);
				ensure!(origin == details.owner, Error::<T, I>::NoPermission);

				details.issuer = issuer.clone();
				details.admin = admin.clone();
				details.freezer = freezer.clone();

				Self::deposit_event(Event::TeamChanged { asset_id: id, issuer, admin, freezer });
				Ok(())
			})
		}

		/// Set the metadata for an asset.
		///
		/// Origin must be Signed and the sender should be the Owner of the asset `id`.
		///
		/// Funds of sender are reserved according to the formula:
		/// `MetadataDepositBase + MetadataDepositPerByte * (name.len + symbol.len)` taking into
		/// account any already reserved funds.
		///
		/// - `id`: The identifier of the asset to update.
		/// - `name`: The user friendly name of this asset. Limited in length by `StringLimit`.
		/// - `symbol`: The exchange symbol for this asset. Limited in length by `StringLimit`.
		/// - `decimals`: The number of decimals this asset uses to represent one unit.
		///
		/// Emits `MetadataSet`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(17)]
		#[pallet::weight(T::WeightInfo::set_metadata(name.len() as u32, symbol.len() as u32))]
		pub fn set_metadata(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			name: Vec<u8>,
			symbol: Vec<u8>,
			decimals: u8,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();
			Self::do_set_metadata(id, &origin, name, symbol, decimals)
		}

		/// Clear the metadata for an asset.
		///
		/// Origin must be Signed and the sender should be the Owner of the asset `id`.
		///
		/// Any deposit is freed for the asset owner.
		///
		/// - `id`: The identifier of the asset to clear.
		///
		/// Emits `MetadataCleared`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(18)]
		pub fn clear_metadata(origin: OriginFor<T>, id: T::AssetIdParameter) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			let d = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			ensure!(d.status == AssetStatus::Live, Error::<T, I>::AssetNotLive);
			ensure!(origin == d.owner, Error::<T, I>::NoPermission);

			Metadata::<T, I>::try_mutate_exists(id.clone(), |metadata| {
				let deposit = metadata.take().ok_or(Error::<T, I>::Unknown)?.deposit;
				T::Currency::unreserve(&d.owner, deposit);
				Self::deposit_event(Event::MetadataCleared { asset_id: id });
				Ok(())
			})
		}

		/// Force the metadata for an asset to some value.
		///
		/// Origin must be ForceOrigin.
		///
		/// Any deposit is left alone.
		///
		/// - `id`: The identifier of the asset to update.
		/// - `name`: The user friendly name of this asset. Limited in length by `StringLimit`.
		/// - `symbol`: The exchange symbol for this asset. Limited in length by `StringLimit`.
		/// - `decimals`: The number of decimals this asset uses to represent one unit.
		///
		/// Emits `MetadataSet`.
		///
		/// Weight: `O(N + S)` where N and S are the length of the name and symbol respectively.
		#[pallet::call_index(19)]
		#[pallet::weight(T::WeightInfo::force_set_metadata(name.len() as u32, symbol.len() as u32))]
		pub fn force_set_metadata(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			name: Vec<u8>,
			symbol: Vec<u8>,
			decimals: u8,
			is_frozen: bool,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			let id: T::AssetId = id.into();

			let bounded_name: BoundedVec<u8, T::StringLimit> =
				name.clone().try_into().map_err(|_| Error::<T, I>::BadMetadata)?;

			let bounded_symbol: BoundedVec<u8, T::StringLimit> =
				symbol.clone().try_into().map_err(|_| Error::<T, I>::BadMetadata)?;

			ensure!(Asset::<T, I>::contains_key(&id), Error::<T, I>::Unknown);
			Metadata::<T, I>::try_mutate_exists(id.clone(), |metadata| {
				let deposit = metadata.take().map_or(Zero::zero(), |m| m.deposit);
				*metadata = Some(AssetMetadata {
					deposit,
					name: bounded_name,
					symbol: bounded_symbol,
					decimals,
					is_frozen,
				});

				Self::deposit_event(Event::MetadataSet {
					asset_id: id,
					name,
					symbol,
					decimals,
					is_frozen,
				});
				Ok(())
			})
		}

		/// Clear the metadata for an asset.
		///
		/// Origin must be ForceOrigin.
		///
		/// Any deposit is returned.
		///
		/// - `id`: The identifier of the asset to clear.
		///
		/// Emits `MetadataCleared`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(20)]
		pub fn force_clear_metadata(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			let id: T::AssetId = id.into();

			let d = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			Metadata::<T, I>::try_mutate_exists(id.clone(), |metadata| {
				let deposit = metadata.take().ok_or(Error::<T, I>::Unknown)?.deposit;
				T::Currency::unreserve(&d.owner, deposit);
				Self::deposit_event(Event::MetadataCleared { asset_id: id });
				Ok(())
			})
		}

		/// Alter the attributes of a given asset.
		///
		/// Origin must be `ForceOrigin`.
		///
		/// - `id`: The identifier of the asset.
		/// - `owner`: The new Owner of this asset.
		/// - `issuer`: The new Issuer of this asset.
		/// - `admin`: The new Admin of this asset.
		/// - `freezer`: The new Freezer of this asset.
		/// - `min_balance`: The minimum balance of this new asset that any single account must
		/// have. If an account's balance is reduced below this, then it collapses to zero.
		/// - `is_sufficient`: Whether a non-zero balance of this asset is deposit of sufficient
		/// value to account for the state bloat associated with its balance storage. If set to
		/// `true`, then non-zero balances may be stored without a `consumer` reference (and thus
		/// an ED in the Balances pallet or whatever else is used to control user-account state
		/// growth).
		/// - `is_frozen`: Whether this asset class is frozen except for permissioned/admin
		/// instructions.
		///
		/// Emits `AssetStatusChanged` with the identity of the asset.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(21)]
		pub fn force_asset_status(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			owner: AccountIdLookupOf<T>,
			issuer: AccountIdLookupOf<T>,
			admin: AccountIdLookupOf<T>,
			freezer: AccountIdLookupOf<T>,
			#[pallet::compact] min_balance: T::Balance,
			is_sufficient: bool,
			is_frozen: bool,
		) -> DispatchResult {
			T::ForceOrigin::ensure_origin(origin)?;
			let id: T::AssetId = id.into();

			Asset::<T, I>::try_mutate(id.clone(), |maybe_asset| {
				let mut asset = maybe_asset.take().ok_or(Error::<T, I>::Unknown)?;
				ensure!(asset.status != AssetStatus::Destroying, Error::<T, I>::AssetNotLive);
				asset.owner = T::Lookup::lookup(owner)?;
				asset.issuer = T::Lookup::lookup(issuer)?;
				asset.admin = T::Lookup::lookup(admin)?;
				asset.freezer = T::Lookup::lookup(freezer)?;
				asset.min_balance = min_balance;
				asset.is_sufficient = is_sufficient;
				if is_frozen {
					asset.status = AssetStatus::Frozen;
				} else {
					asset.status = AssetStatus::Live;
				}
				*maybe_asset = Some(asset);

				Self::deposit_event(Event::AssetStatusChanged { asset_id: id });
				Ok(())
			})
		}

		/// Approve an amount of asset for transfer by a delegated third-party account.
		///
		/// Origin must be Signed.
		///
		/// Ensures that `ApprovalDeposit` worth of `Currency` is reserved from signing account
		/// for the purpose of holding the approval. If some non-zero amount of assets is already
		/// approved from signing account to `delegate`, then it is topped up or unreserved to
		/// meet the right value.
		///
		/// NOTE: The signing account does not need to own `amount` of assets at the point of
		/// making this call.
		///
		/// - `id`: The identifier of the asset.
		/// - `delegate`: The account to delegate permission to transfer asset.
		/// - `amount`: The amount of asset that may be transferred by `delegate`. If there is
		/// already an approval in place, then this acts additively.
		///
		/// Emits `ApprovedTransfer` on success.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(22)]
		pub fn approve_transfer(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			delegate: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let owner = ensure_signed(origin)?;
			let delegate = T::Lookup::lookup(delegate)?;
			let id: T::AssetId = id.into();
			Self::do_approve_transfer(id, &owner, &delegate, amount)
		}

		/// Cancel all of some asset approved for delegated transfer by a third-party account.
		///
		/// Origin must be Signed and there must be an approval in place between signer and
		/// `delegate`.
		///
		/// Unreserves any deposit previously reserved by `approve_transfer` for the approval.
		///
		/// - `id`: The identifier of the asset.
		/// - `delegate`: The account delegated permission to transfer asset.
		///
		/// Emits `ApprovalCancelled` on success.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(23)]
		pub fn cancel_approval(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			delegate: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let owner = ensure_signed(origin)?;
			let delegate = T::Lookup::lookup(delegate)?;
			let id: T::AssetId = id.into();
			Self::do_cancel_approval(&id, &owner, &delegate)
		}

		/// Cancel all of some asset approved for delegated transfer by a third-party account.
		///
		/// Origin must be either ForceOrigin or Signed origin with the signer being the Admin
		/// account of the asset `id`.
		///
		/// Unreserves any deposit previously reserved by `approve_transfer` for the approval.
		///
		/// - `id`: The identifier of the asset.
		/// - `delegate`: The account delegated permission to transfer asset.
		///
		/// Emits `ApprovalCancelled` on success.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(24)]
		pub fn force_cancel_approval(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			owner: AccountIdLookupOf<T>,
			delegate: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let id: T::AssetId = id.into();
			let d = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			T::ForceOrigin::try_origin(origin)
				.map(|_| ())
				.or_else(|origin| -> DispatchResult {
					let origin = ensure_signed(origin)?;
					ensure!(origin == d.admin, Error::<T, I>::NoPermission);
					Ok(())
				})?;

			let owner = T::Lookup::lookup(owner)?;
			let delegate = T::Lookup::lookup(delegate)?;
			Self::do_cancel_approval(&id, &owner, &delegate)
		}

		/// Transfer some asset balance from a previously delegated account to some third-party
		/// account.
		///
		/// Origin must be Signed and there must be an approval in place by the `owner` to the
		/// signer.
		///
		/// If the entire amount approved for transfer is transferred, then any deposit previously
		/// reserved by `approve_transfer` is unreserved.
		///
		/// - `id`: The identifier of the asset.
		/// - `owner`: The account which previously approved for a transfer of at least `amount` and
		/// from which the asset balance will be withdrawn.
		/// - `destination`: The account to which the asset balance of `amount` will be transferred.
		/// - `amount`: The amount of assets to transfer.
		///
		/// Emits `TransferredApproved` on success.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(25)]
		pub fn transfer_approved(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			owner: AccountIdLookupOf<T>,
			destination: AccountIdLookupOf<T>,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let delegate = ensure_signed(origin)?;
			let owner = T::Lookup::lookup(owner)?;
			let destination = T::Lookup::lookup(destination)?;
			let id: T::AssetId = id.into();
			Self::do_transfer_approved(id, &owner, &delegate, &destination, amount)
		}

		/// Create an asset account for non-provider assets.
		///
		/// A deposit will be taken from the signer account.
		///
		/// - `origin`: Must be Signed; the signer account must have sufficient funds for a deposit
		///   to be taken.
		/// - `id`: The identifier of the asset for the account to be created.
		///
		/// Emits `Touched` event when successful.
		#[pallet::call_index(26)]
		#[pallet::weight(T::WeightInfo::touch())]
		pub fn touch(origin: OriginFor<T>, id: T::AssetIdParameter) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let id: T::AssetId = id.into();
			Self::do_touch(id, who.clone(), who)
		}

		/// Return the deposit (if any) of an asset account or a consumer reference (if any) of an
		/// account.
		///
		/// The origin must be Signed.
		///
		/// - `id`: The identifier of the asset for which the caller would like the deposit
		///   refunded.
		/// - `allow_burn`: If `true` then assets may be destroyed in order to complete the refund.
		///
		/// It will fail with either [`Error::ContainsHolds`] or [`Error::ContainsFreezes`] if
		/// the asset account contains holds or freezes in place.
		///
		/// Emits `Refunded` event when successful.
		#[pallet::call_index(27)]
		#[pallet::weight(T::WeightInfo::refund())]
		pub fn refund(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			allow_burn: bool,
		) -> DispatchResult {
			let id: T::AssetId = id.into();
			Self::do_refund(id, ensure_signed(origin)?, allow_burn)
		}

		/// Sets the minimum balance of an asset.
		///
		/// Only works if there aren't any accounts that are holding the asset or if
		/// the new value of `min_balance` is less than the old one.
		///
		/// Origin must be Signed and the sender has to be the Owner of the
		/// asset `id`.
		///
		/// - `id`: The identifier of the asset.
		/// - `min_balance`: The new value of `min_balance`.
		///
		/// Emits `AssetMinBalanceChanged` event when successful.
		#[pallet::call_index(28)]
		pub fn set_min_balance(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			min_balance: T::Balance,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			let mut details = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			ensure!(origin == details.owner, Error::<T, I>::NoPermission);

			let old_min_balance = details.min_balance;
			// If the asset is marked as sufficient it won't be allowed to
			// change the min_balance.
			ensure!(!details.is_sufficient, Error::<T, I>::NoPermission);

			// Ensure that either the new min_balance is less than old
			// min_balance or there aren't any accounts holding the asset.
			ensure!(
				min_balance < old_min_balance || details.accounts == 0,
				Error::<T, I>::NoPermission
			);

			details.min_balance = min_balance;
			Asset::<T, I>::insert(&id, details);

			Self::deposit_event(Event::AssetMinBalanceChanged {
				asset_id: id,
				new_min_balance: min_balance,
			});
			Ok(())
		}

		/// Create an asset account for `who`.
		///
		/// A deposit will be taken from the signer account.
		///
		/// - `origin`: Must be Signed; the signer account must have sufficient funds for a deposit
		///   to be taken.
		/// - `id`: The identifier of the asset for the account to be created, the asset status must
		///   be live.
		/// - `who`: The account to be created.
		///
		/// Emits `Touched` event when successful.
		#[pallet::call_index(29)]
		#[pallet::weight(T::WeightInfo::touch_other())]
		pub fn touch_other(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			who: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let who = T::Lookup::lookup(who)?;
			let id: T::AssetId = id.into();
			Self::do_touch(id, who, origin)
		}

		/// Return the deposit (if any) of a target asset account. Useful if you are the depositor.
		///
		/// The origin must be Signed and either the account owner, depositor, or asset `Admin`. In
		/// order to burn a non-zero balance of the asset, the caller must be the account and should
		/// use `refund`.
		///
		/// - `id`: The identifier of the asset for the account holding a deposit.
		/// - `who`: The account to refund.
		///
		/// It will fail with either [`Error::ContainsHolds`] or [`Error::ContainsFreezes`] if
		/// the asset account contains holds or freezes in place.
		///
		/// Emits `Refunded` event when successful.
		#[pallet::call_index(30)]
		#[pallet::weight(T::WeightInfo::refund_other())]
		pub fn refund_other(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			who: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let who = T::Lookup::lookup(who)?;
			let id: T::AssetId = id.into();
			Self::do_refund_other(id, &who, Some(origin))
		}

		/// Disallow further unprivileged transfers of an asset `id` to and from an account `who`.
		///
		/// Origin must be Signed and the sender should be the Freezer of the asset `id`.
		///
		/// - `id`: The identifier of the account's asset.
		/// - `who`: The account to be unblocked.
		///
		/// Emits `Blocked`.
		///
		/// Weight: `O(1)`
		#[pallet::call_index(31)]
		pub fn block(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			who: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let id: T::AssetId = id.into();

			let d = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			ensure!(
				d.status == AssetStatus::Live || d.status == AssetStatus::Frozen,
				Error::<T, I>::IncorrectStatus
			);
			ensure!(origin == d.freezer, Error::<T, I>::NoPermission);
			let who = T::Lookup::lookup(who)?;

			Account::<T, I>::try_mutate(&id, &who, |maybe_account| -> DispatchResult {
				maybe_account.as_mut().ok_or(Error::<T, I>::NoAccount)?.status =
					AccountStatus::Blocked;
				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::Blocked { asset_id: id, who });
			Ok(())
		}

		/// Transfer the entire transferable balance from the caller asset account.
		///
		/// NOTE: This function only attempts to transfer _transferable_ balances. This means that
		/// any held, frozen, or minimum balance (when `keep_alive` is `true`), will not be
		/// transferred by this function. To ensure that this function results in a killed account,
		/// you might need to prepare the account by removing any reference counters, storage
		/// deposits, etc...
		///
		/// The dispatch origin of this call must be Signed.
		///
		/// - `id`: The identifier of the asset for the account holding a deposit.
		/// - `dest`: The recipient of the transfer.
		/// - `keep_alive`: A boolean to determine if the `transfer_all` operation should send all
		///   of the funds the asset account has, causing the sender asset account to be killed
		///   (false), or transfer everything except at least the minimum balance, which will
		///   guarantee to keep the sender asset account alive (true).
		#[pallet::call_index(32)]
		#[pallet::weight(T::WeightInfo::transfer_all())]
		pub fn transfer_all(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			dest: AccountIdLookupOf<T>,
			keep_alive: bool,
		) -> DispatchResult {
			let transactor = ensure_signed(origin)?;
			let keep_alive = if keep_alive { Preserve } else { Expendable };
			let reducible_balance = <Self as fungibles::Inspect<_>>::reducible_balance(
				id.clone().into(),
				&transactor,
				keep_alive,
				Fortitude::Polite,
			);
			let dest = T::Lookup::lookup(dest)?;
			<Self as fungibles::Mutate<_>>::transfer(
				id.into(),
				&transactor,
				&dest,
				reducible_balance,
				keep_alive,
			)?;
			Ok(())
		}

		/// Sets the trusted reserve information of an asset.
		///
		/// Origin must be the Owner of the asset `id`. The origin must conform to the configured
		/// `CreateOrigin` or be the signed `owner` configured during asset creation.
		///
		/// - `id`: The identifier of the asset.
		/// - `reserves`: The full list of trusted reserves information.
		///
		/// Emits `AssetMinBalanceChanged` event when successful.
		#[pallet::call_index(33)]
		#[pallet::weight(T::WeightInfo::set_reserves(reserves.len() as u32))]
		pub fn set_reserves(
			origin: OriginFor<T>,
			id: T::AssetIdParameter,
			reserves: BoundedVec<T::ReserveData, ConstU32<MAX_RESERVES>>,
		) -> DispatchResult {
			let id: T::AssetId = id.into();
			let origin = ensure_signed(origin.clone())
				.or_else(|_| T::CreateOrigin::ensure_origin(origin, &id))?;

			let details = Asset::<T, I>::get(&id).ok_or(Error::<T, I>::Unknown)?;
			ensure!(origin == details.owner, Error::<T, I>::NoPermission);

			Self::unchecked_update_reserves(id, reserves)?;
			Ok(())
		}
	}

	#[pallet::view_functions]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Provide the asset details for asset `id`.
		pub fn asset_details(
			id: T::AssetId,
		) -> Option<AssetDetails<T::Balance, T::AccountId, DepositBalanceOf<T, I>>> {
			Asset::<T, I>::get(id)
		}

		/// Provide the balance of `who` for asset `id`.
		pub fn balance_of(who: T::AccountId, id: T::AssetId) -> Option<<T as Config<I>>::Balance> {
			Account::<T, I>::get(id, who).map(|account| account.balance)
		}

		/// Provide the configured metadata for asset `id`.
		pub fn get_metadata(
			id: T::AssetId,
		) -> Option<AssetMetadata<DepositBalanceOf<T, I>, BoundedVec<u8, T::StringLimit>>> {
			Metadata::<T, I>::try_get(id).ok()
		}

		/// Provide the configured reserves data for asset `id`.
		pub fn get_reserves_data(id: T::AssetId) -> Vec<T::ReserveData> {
			Self::reserves(&id)
		}
	}

	/// Implements [`AccountTouch`] trait.
	/// Note that a depositor can be any account, without any specific privilege.
	impl<T: Config<I>, I: 'static> AccountTouch<T::AssetId, T::AccountId> for Pallet<T, I> {
		type Balance = DepositBalanceOf<T, I>;

		fn deposit_required(_: T::AssetId) -> Self::Balance {
			T::AssetAccountDeposit::get()
		}

		fn should_touch(asset: T::AssetId, who: &T::AccountId) -> bool {
			match Asset::<T, I>::get(&asset) {
				// refer to the [`Self::new_account`] function for more details.
				Some(info) if info.is_sufficient => false,
				Some(_) if frame_system::Pallet::<T>::can_accrue_consumers(who, 2) => false,
				Some(_) => !Account::<T, I>::contains_key(asset, who),
				_ => true,
			}
		}

		fn touch(
			asset: T::AssetId,
			who: &T::AccountId,
			depositor: &T::AccountId,
		) -> DispatchResult {
			Self::do_touch(asset, who.clone(), depositor.clone())
		}
	}

	/// Implements [`ContainsPair`] trait for a pair of asset and account IDs.
	impl<T: Config<I>, I: 'static> ContainsPair<T::AssetId, T::AccountId> for Pallet<T, I> {
		/// Check if an account with the given asset ID and account address exists.
		fn contains(asset: &T::AssetId, who: &T::AccountId) -> bool {
			Account::<T, I>::contains_key(asset, who)
		}
	}

	/// Implements [`ProvideAssetReserves`] trait for getting the list of trusted reserves for a
	/// given asset.
	impl<T: Config<I>, I: 'static> ProvideAssetReserves<T::AssetId, T::ReserveData> for Pallet<T, I> {
		/// Provide the configured reserves for asset `id`.
		fn reserves(id: &T::AssetId) -> Vec<T::ReserveData> {
			Reserves::<T, I>::get(id).into_inner()
		}
	}
}

#[cfg(any(feature = "try-runtime", test))]
impl<T: Config<I>, I: 'static> Pallet<T, I> {
	pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
		for asset_id in Reserves::<T, I>::iter_keys() {
			ensure!(Asset::<T, I>::contains_key(asset_id.clone()), "Orphaned Reserves data found");
		}

		for asset_id in Metadata::<T, I>::iter_keys() {
			ensure!(Asset::<T, I>::contains_key(asset_id.clone()), "Orphaned Metadata found");
		}

		for (asset_id, _, _) in Approvals::<T, I>::iter_keys() {
			ensure!(Asset::<T, I>::contains_key(asset_id.clone()), "Orphaned Approval found");
		}

		for (asset_id, _) in Account::<T, I>::iter_keys() {
			ensure!(Asset::<T, I>::contains_key(asset_id.clone()), "Orphaned Account found");
		}

		for (asset_id, details) in Asset::<T, I>::iter() {
			if details.status == AssetStatus::Destroying {
				continue;
			}

			let mut calculated_supply = T::Balance::zero();
			let mut calculated_accounts = 0u32;
			let mut calculated_sufficients = 0u32;

			for (who, account) in Account::<T, I>::iter_prefix(&asset_id) {
				let held = T::Holder::balance_on_hold(asset_id.clone(), &who).unwrap_or_default();
				calculated_supply =
					calculated_supply.saturating_add(account.balance).saturating_add(held);
				calculated_accounts += 1;

				if matches!(account.reason, ExistenceReason::Sufficient) {
					calculated_sufficients += 1;
				}

				let total_balance = account.balance.saturating_add(held);
				if total_balance < details.min_balance {
					if !matches!(
						account.reason,
						ExistenceReason::DepositHeld(_) | ExistenceReason::DepositFrom(_, _)
					) {
						log::warn!(
							"Account {who:?} for asset {asset_id:?} has total balance below min_balance but no deposit. Balance: {:?}, Held: {:?}, Min balance: {:?}, Reason: {:?}",
							account.balance,
							held,
							details.min_balance,
							account.reason,
						);
					}
				}
			}

			// Using >= instead of == because the provided `do_refund` implementation
			// historically destroyed the account balance without decrementing the asset
			// supply. Although this has been fixed, existing on-chain state may still
			// contain overcounted supply from prior refunds.
			// TODO: add a migration to recalculate supply, then tighten this to `==`.
			ensure!(details.supply >= calculated_supply, "Asset supply mismatch");
			if details.accounts != calculated_accounts {
				// Legacy error in Kusama Asset Hub that needs to be cleaned up.
				log::error!(
					"Asset {asset_id:?} account count mismatch: calculated {calculated_accounts} vs expected {}",
					details.accounts,
				);
			}
			ensure!(
				details.sufficients == calculated_sufficients,
				"Asset sufficients count mismatch"
			);

			let calculated_approvals = Approvals::<T, I>::iter_prefix((&asset_id,)).count() as u32;

			if details.approvals != calculated_approvals {
				log::error!(
					"Asset {asset_id:?} approvals count mismatch: calculated {calculated_approvals} vs expected {}",
					details.approvals,
				);

				return Err("Asset approvals count mismatch".into());
			}
		}
		Ok(())
	}
}

sp_core::generate_feature_enabled_macro!(runtime_benchmarks_enabled, feature = "runtime-benchmarks", $);
