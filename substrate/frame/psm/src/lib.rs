// This file is part of Substrate.

// Copyright (C) Amforc AG.
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

//! # Peg Stability Module (PSM) Pallet
//!
//! Instantiable Peg Stability Modules (PSMs). Each PSM enables 1:1 swaps between an internal
//! stablecoin and one or more approved external stablecoins, typically to maintain a peg.
//!
//! ## Pallet API
//!
//! See the [`pallet`] module for more information about the interfaces this pallet exposes,
//! including its configuration trait, dispatchables, storage items, events and errors.
//!
//! ## Terminology
//!
//! Throughout this pallet two distinct token roles are referenced:
//!
//! * **Internal** — the stablecoin a PSM issues and burns (e.g. a runtime's own USD-pegged
//!   stablecoin). Each PSM instance is keyed by its internal asset id; multiple instances can
//!   coexist, each with its own reserve, debt ceiling, fee destination and approved externals. Mint
//!   operations credit the user with the internal asset; redeem operations burn it. Fees are
//!   collected in the internal asset and forwarded to that instance's [`PsmInfo::fee_destination`].
//! * **External** — third-party assets (e.g. USDC, USDT) approved on a specific PSM via
//!   [`Pallet::add_external_asset`] and held in that PSM's reserve. Users deposit external to mint
//!   internal, and burn internal to redeem external. A PSM may approve multiple externals, each
//!   identified by `external_asset`.
//!
//! ## Overview
//!
//! A PSM strengthens its internal asset's peg by providing arbitrage opportunities:
//! - When the internal asset trades **above** $1: Users swap external assets for the internal asset
//!   and sell for profit.
//! - When the internal asset trades **below** $1: Users buy cheap internal asset and swap for
//!   external assets.
//!
//! This creates a price corridor bounded by the minting and redemption fees.
//!
//! ### Key Concepts
//!
//! * **PSM instance**: A configured Peg Stability Module, keyed by its internal asset id and
//!   described by [`PsmInfo`]. Each instance has its own reserve account derived from
//!   `blake2_256((PalletId::TYPE_ID, PalletId, internal_asset).encode())`.
//! * **Minting**: Deposit external asset → receive internal asset (minus fee).
//! * **Redemption**: Burn internal asset → receive external asset (minus fee).
//! * **Reserve**: External asset balance held by a PSM's reserve account (derived, not stored).
//! * **PSM Debt**: Total internal asset minted through a PSM, backed 1:1 by external assets in that
//!   PSM's reserve.
//! * **Circuit Breaker**: Per-external emergency control to disable minting or all swaps.
//!
//! ### Fee Structure
//!
//! * **Minting Fee (`MintingFee`)**: Deducted from internal-asset output during minting, configured
//!   per `(internal_asset, external_asset)` pair.
//! * **Redemption Fee (`RedemptionFee`)**: Deducted from external-asset output during redemption,
//!   configured per `(internal_asset, external_asset)` pair.
//!
//! Fees are collected in the internal asset and transferred to the instance's
//! [`PsmInfo::fee_destination`].
//!
//! ### Example
//!
//! ```ignore
//! // Mint internal asset by depositing USDC on the PSM
//! let max_fee = MintingFee::<Runtime>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
//! Psm::mint(
//! 	RuntimeOrigin::signed(user),
//! 	INTERNAL_ASSET_ID,
//! 	USDC_ASSET_ID,
//! 	1000 * UNIT,
//! 	max_fee,
//! )?;
//!
//! // Redeem USDC by burning the internal asset
//! let max_fee = RedemptionFee::<Runtime>::get(INTERNAL_ASSET_ID, USDC_ASSET_ID);
//! Psm::redeem(
//! 	RuntimeOrigin::signed(user),
//! 	INTERNAL_ASSET_ID,
//! 	USDC_ASSET_ID,
//! 	1000 * UNIT,
//! 	max_fee,
//! )?;
//! ```

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod weights;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use pallet::*;
pub use weights::WeightInfo;

/// Helper trait for benchmark setup.
///
/// Provides a way to create an external asset with the correct metadata (decimals)
/// for benchmarks, abstracting over the deposit requirements of the underlying
/// asset pallet.
#[cfg(feature = "runtime-benchmarks")]
pub trait BenchmarkHelper<AssetId, AccountId> {
	/// Get the asset ID for a given asset index.
	fn get_asset_id(asset_index: u32) -> AssetId;
	/// Create an asset with metadata matching the internal asset's decimals.
	fn create_asset(asset_id: AssetId, owner: &AccountId, decimals: u8);
}

#[frame_support::pallet]
pub mod pallet {

	use alloc::boxed::Box;
	use codec::DecodeWithMemTracking;
	use frame_support::{
		pallet_prelude::*,
		traits::{
			fungibles::{
				metadata::Inspect as FungiblesMetadataInspect,
				roles::Inspect as FungiblesRolesInspect, Inspect as FungiblesInspect,
				Mutate as FungiblesMutate,
			},
			tokens::{Fortitude, Precision, Preservation},
			CallerTrait, Consideration, EnsureOriginWithArg, Footprint, OriginTrait,
		},
		PalletId,
	};
	use frame_system::pallet_prelude::*;
	use sp_runtime::{
		traits::{CheckedDiv, CheckedMul, Saturating, TrailingZeroInput, Zero},
		Perbill, Permill, TypeId,
	};

	use crate::WeightInfo;

	/// Circuit breaker levels for emergency control.
	#[derive(
		Encode,
		Decode,
		DecodeWithMemTracking,
		MaxEncodedLen,
		TypeInfo,
		Clone,
		Copy,
		PartialEq,
		Eq,
		Debug,
		Default,
	)]
	pub enum CircuitBreakerLevel {
		/// Normal operation, all swaps enabled.
		#[default]
		AllEnabled,
		/// Minting disabled, redemptions still allowed.
		MintingDisabled,
		/// All swaps disabled.
		AllDisabled,
	}

	impl CircuitBreakerLevel {
		/// Whether this level allows minting (external → internal).
		pub const fn allows_minting(&self) -> bool {
			matches!(self, CircuitBreakerLevel::AllEnabled)
		}

		/// Whether this level allows redemption (internal → external).
		pub const fn allows_redemption(&self) -> bool {
			!matches!(self, CircuitBreakerLevel::AllDisabled)
		}
	}

	/// Privilege level of an origin acting on a PSM instance.
	///
	/// Resolved by matching the incoming origin against the instance's stored
	/// [`PsmAdminInfo::full_admin`] (`Full`) or [`PsmAdminInfo::emergency_admin`]
	/// (`Emergency`), enabling tiered authorization over the instance's parameters.
	#[derive(
		Encode,
		Decode,
		DecodeWithMemTracking,
		MaxEncodedLen,
		TypeInfo,
		Clone,
		Copy,
		PartialEq,
		Eq,
		Debug,
		Default,
	)]
	pub enum PsmManagerLevel {
		/// Full administrative access, held by the instance's `full_admin`.
		/// Can modify all parameters including fees, ceilings, and asset management,
		/// reassign admins, and remove the instance.
		#[default]
		Full,
		/// Emergency access, held by the instance's `emergency_admin`.
		/// Can modify circuit breaker status.
		Emergency,
	}

	impl PsmManagerLevel {
		/// Whether this level allows modifying minting/redemption fees.
		pub const fn can_set_fees(&self) -> bool {
			matches!(self, PsmManagerLevel::Full)
		}

		/// Whether this level allows modifying the circuit breaker status.
		/// Both Full and Emergency levels can set circuit breaker.
		pub const fn can_set_circuit_breaker(&self) -> bool {
			matches!(self, PsmManagerLevel::Full | PsmManagerLevel::Emergency)
		}

		/// Whether this level allows modifying the PSM debt ceiling.
		/// Only Full can set the debt ceiling.
		pub const fn can_set_max_debt(&self) -> bool {
			matches!(self, PsmManagerLevel::Full)
		}

		/// Whether this level allows modifying per-asset ceiling weights.
		/// Only Full can set asset ceiling weights.
		pub const fn can_set_asset_ceiling(&self) -> bool {
			matches!(self, PsmManagerLevel::Full)
		}

		/// Whether this level allows adding or removing external assets.
		pub const fn can_manage_assets(&self) -> bool {
			matches!(self, PsmManagerLevel::Full)
		}

		/// Whether this level allows reassigning the PSM's `full_admin` / `emergency_admin`.
		pub const fn can_manage_admins(&self) -> bool {
			matches!(self, PsmManagerLevel::Full)
		}

		/// Whether this level allows removing the PSM instance.
		pub const fn can_remove_psm(&self) -> bool {
			matches!(self, PsmManagerLevel::Full)
		}
	}

	pub(crate) type BalanceOf<T> = <<T as Config>::Fungibles as FungiblesInspect<
		<T as frame_system::Config>::AccountId,
	>>::Balance;

	/// Suggested fee of 0.5% for minting and redemption.
	pub(crate) struct DefaultFee;
	impl Get<Permill> for DefaultFee {
		fn get() -> Permill {
			Permill::from_parts(5_000)
		}
	}

	/// Maximum absolute difference between an external asset's decimals and the internal
	/// asset's decimals. Bounds the scaling factor `10^diff` well below `u128::MAX`
	/// so realistic balances cannot overflow during conversion.
	pub const MAX_DECIMALS_DIFF: u32 = 24;

	/// On-chain record of a PSM instance.
	#[derive(
		Encode, Decode, DecodeWithMemTracking, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug,
	)]
	#[scale_info(skip_type_params(T))]
	pub struct PsmInfo<T: Config> {
		/// Account receiving minting and redemption fees, denominated in the internal asset.
		pub fee_destination: T::AccountId,
		/// This PSM instance's debt ceiling, in internal-asset units.
		pub max_debt: BalanceOf<T>,
		/// Minimum swap amount for this instance, in internal-asset units. Swaps whose
		/// internal-equivalent falls below this are rejected with [`Error::BelowMinimumSwap`].
		pub min_swap_amount: BalanceOf<T>,
		/// Snapshot of the internal asset's decimals at install time.
		pub internal_decimals: u8,
		/// Number of approved external assets attached to this instance.
		pub external_count: u32,
	}

	/// Admin origins and creation-deposit bookkeeping for a PSM instance. Always written
	/// and removed together with the [`Psm`] entry.
	#[derive(Encode, Decode, MaxEncodedLen, TypeInfo, Clone, PartialEq, Eq, Debug)]
	#[scale_info(skip_type_params(T))]
	// Kept separate from `PsmInfo` so the swap path (`mint`/`redeem`) doesn't read the
	// admin origins, which can be large.
	pub struct PsmAdminInfo<T: Config> {
		/// Origin with `Full` management privileges over this PSM. Set on `create_psm` and
		/// reassignable to any origin via `set_full_admin`.
		pub full_admin: T::PalletsOrigin,
		/// Origin with `Emergency` management privileges over this PSM. Set on `create_psm` and
		/// reassignable to any origin via `set_emergency_admin`.
		pub emergency_admin: T::PalletsOrigin,
		/// Optional creation deposit and its depositor. Dropped on `remove_psm`, independently of
		/// any admin reassignment.
		pub deposit: Option<(T::AccountId, T::Consideration)>,
	}

	/// On-chain record of an external asset approved on a PSM instance.
	#[derive(
		Encode,
		Decode,
		DecodeWithMemTracking,
		MaxEncodedLen,
		TypeInfo,
		Clone,
		Copy,
		PartialEq,
		Eq,
		Debug,
	)]
	pub struct ExternalAssetInfo {
		/// Per-external circuit breaker status.
		pub status: CircuitBreakerLevel,
		/// Snapshot of the external asset's decimals at registration time.
		pub decimals: u8,
	}

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Fungibles implementation for both internal and external assets.
		type Fungibles: FungiblesMutate<Self::AccountId, AssetId = Self::AssetId>
			+ FungiblesMetadataInspect<Self::AccountId>
			+ FungiblesRolesInspect<Self::AccountId>;

		/// Consideration for PSM creation. Runtimes can price this from the PSM footprint or
		/// anything else they choose.
		type Consideration: Consideration<Self::AccountId, Footprint>;

		/// Origin permitted to create a PSM for a given internal asset; succeeds with the
		/// optional account that pays the creation consideration. Returning `None` creates without
		/// a deposit, useful for privileged origins such as Root.
		type CreateOrigin: EnsureOriginWithArg<
			<Self as frame_system::Config>::RuntimeOrigin,
			Self::AssetId,
			Success = Option<Self::AccountId>,
		>;

		/// The aggregated origin, tying the runtime origin to [`Config::PalletsOrigin`] so PSM
		/// admins can be matched against incoming origins.
		type RuntimeOrigin: OriginTrait<PalletsOrigin = Self::PalletsOrigin>
			+ From<Self::PalletsOrigin>
			+ IsType<<Self as frame_system::Config>::RuntimeOrigin>;

		/// The caller origin, overarching type of all pallets' origins. Stored as a PSM's
		/// `full_admin` / `emergency_admin` and matched against incoming origins.
		type PalletsOrigin: Parameter
			+ From<frame_system::RawOrigin<Self::AccountId>>
			+ CallerTrait<Self::AccountId>
			+ MaxEncodedLen;

		/// Asset identifier type.
		type AssetId: Parameter + Member + Clone + MaybeSerializeDeserialize + MaxEncodedLen + Ord;

		/// A type representing the weights required by the dispatchables of this pallet.
		type WeightInfo: WeightInfo;

		/// PalletId for deriving each PSM instance's reserve sub-account.
		#[pallet::constant]
		type PalletId: Get<PalletId>;

		/// Maximum number of approved external assets per PSM instance.
		#[pallet::constant]
		type MaxExternals: Get<u32>;

		/// Helper for benchmarks to create an external asset with correct metadata.
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: crate::BenchmarkHelper<Self::AssetId, Self::AccountId>;
	}

	/// [`Config::CreateOrigin`] admitting a signed origin only when it owns the internal asset.
	/// Prevents creating a PSM over an asset you don't control (PSM mint/burn bypasses the
	/// asset's issuer checks).
	pub struct EnsureAssetOwner<T>(core::marker::PhantomData<T>);

	impl<T: Config> EnsureOriginWithArg<<T as frame_system::Config>::RuntimeOrigin, T::AssetId>
		for EnsureAssetOwner<T>
	{
		type Success = Option<T::AccountId>;

		fn try_origin(
			origin: <T as frame_system::Config>::RuntimeOrigin,
			internal_asset: &T::AssetId,
		) -> Result<Self::Success, <T as frame_system::Config>::RuntimeOrigin> {
			match ensure_signed(origin.clone()) {
				Ok(who) if T::Fungibles::owner(internal_asset.clone()) == Some(who.clone()) => {
					Ok(Some(who))
				},
				_ => Err(origin),
			}
		}

		#[cfg(feature = "runtime-benchmarks")]
		fn try_successful_origin(
			internal_asset: &T::AssetId,
		) -> Result<<T as frame_system::Config>::RuntimeOrigin, ()> {
			// A signed origin of the asset's current owner satisfies the owner check.
			let owner = T::Fungibles::owner(internal_asset.clone()).ok_or(())?;
			Ok(frame_system::RawOrigin::Signed(owner).into())
		}
	}

	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(0);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T>(_);

	/// A reason for this pallet placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason {
		/// The deposit backing a PSM instance created via `create_psm`.
		#[codec(index = 0)]
		CreationDeposit,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn integrity_test() {
			const ENTROPY_BYTES: usize = 32;
			assert!(
				<T::AccountId as MaxEncodedLen>::max_encoded_len() >= ENTROPY_BYTES,
				"T::AccountId is too small to preserve the full PSM reserve-account entropy",
			);
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}

	/// Registered PSM instances, keyed by the internal asset id.
	#[pallet::storage]
	pub type Psm<T: Config> = StorageMap<_, Blake2_128Concat, T::AssetId, PsmInfo<T>, OptionQuery>;

	/// Admin origins and creation-deposit bookkeeping per PSM, keyed by the internal
	/// asset id. Held separately from [`Psm`] so swaps never decode the admin origins.
	/// Always written and removed together with the corresponding [`Psm`] entry.
	#[pallet::storage]
	pub type PsmAdmin<T: Config> =
		StorageMap<_, Blake2_128Concat, T::AssetId, PsmAdminInfo<T>, OptionQuery>;

	/// Internal-asset debt minted through PSM, per `(internal, external)` pair.
	#[pallet::storage]
	pub type PsmDebt<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		Blake2_128Concat,
		T::AssetId,
		BalanceOf<T>,
		ValueQuery,
	>;

	/// Fee for external → internal swaps (minting), per `(internal, external)` pair.
	/// Defaults to 0.5%.
	#[pallet::storage]
	pub(crate) type MintingFee<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		Blake2_128Concat,
		T::AssetId,
		Permill,
		ValueQuery,
		DefaultFee,
	>;

	/// Fee for internal → external swaps (redemption), per `(internal, external)` pair.
	/// Defaults to 0.5%.
	#[pallet::storage]
	pub(crate) type RedemptionFee<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		Blake2_128Concat,
		T::AssetId,
		Permill,
		ValueQuery,
		DefaultFee,
	>;

	/// Per-external ceiling weight within a PSM, normalised against the sum of weights
	/// for the same instance. Zero disables minting for that external.
	#[pallet::storage]
	pub(crate) type AssetCeilingWeight<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		Blake2_128Concat,
		T::AssetId,
		Permill,
		ValueQuery,
	>;

	/// Approved external assets per PSM.
	#[pallet::storage]
	pub(crate) type ExternalAssets<T: Config> = StorageDoubleMap<
		_,
		Blake2_128Concat,
		T::AssetId,
		Blake2_128Concat,
		T::AssetId,
		ExternalAssetInfo,
		OptionQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// User swapped external asset for internal.
		Minted {
			who: T::AccountId,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			external_consumed: BalanceOf<T>,
			internal_received: BalanceOf<T>,
			internal_fee: BalanceOf<T>,
		},
		/// User swapped internal for external asset.
		Redeemed {
			who: T::AccountId,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			internal_consumed: BalanceOf<T>,
			external_received: BalanceOf<T>,
			internal_fee: BalanceOf<T>,
		},
		/// Minting fee updated for an asset by governance.
		MintingFeeUpdated {
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			old_value: Permill,
			new_value: Permill,
		},
		/// Redemption fee updated for an asset by governance.
		RedemptionFeeUpdated {
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			old_value: Permill,
			new_value: Permill,
		},
		/// PSM debt ceiling updated by governance.
		MaxDebtUpdated {
			internal_asset: T::AssetId,
			old_value: BalanceOf<T>,
			new_value: BalanceOf<T>,
		},
		/// Per-asset debt ceiling weight updated by governance.
		AssetCeilingWeightUpdated {
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			old_value: Permill,
			new_value: Permill,
		},
		/// Per-asset circuit breaker status updated.
		AssetStatusUpdated {
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			status: CircuitBreakerLevel,
		},
		/// An external asset was added to the approved list.
		ExternalAssetAdded { internal_asset: T::AssetId, external_asset: T::AssetId },
		/// An external asset was removed from the approved list.
		ExternalAssetRemoved { internal_asset: T::AssetId, external_asset: T::AssetId },
		/// A PSM instance was created.
		PsmCreated {
			internal_asset: T::AssetId,
			// Boxed: `PalletsOrigin` can be large and would otherwise inflate the `Event` enum.
			full_admin: Box<T::PalletsOrigin>,
			emergency_admin: Box<T::PalletsOrigin>,
			fee_destination: T::AccountId,
			max_debt: BalanceOf<T>,
		},
		/// A PSM instance was removed.
		PsmRemoved { internal_asset: T::AssetId },
		/// A PSM's `full_admin` was reassigned.
		FullAdminChanged {
			internal_asset: T::AssetId,
			old_admin: Box<T::PalletsOrigin>,
			new_admin: Box<T::PalletsOrigin>,
		},
		/// A PSM's `emergency_admin` was reassigned.
		EmergencyAdminChanged {
			internal_asset: T::AssetId,
			old_admin: Box<T::PalletsOrigin>,
			new_admin: Box<T::PalletsOrigin>,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// PSM doesn't have enough external asset for redemption.
		InsufficientReserve,
		/// Swap would exceed PSM debt ceiling.
		ExceedsMaxPsmDebt,
		/// Swap amount below the instance's minimum threshold.
		BelowMinimumSwap,
		/// Current fee exceeds the caller-provided maximum.
		FeeTooHigh,
		/// `create_psm` was called with a zero `min_swap_amount`.
		ZeroMinSwapAmount,
		/// Minting operations are disabled (circuit breaker level >= 1).
		MintingStopped,
		/// All swap operations are disabled (circuit breaker level = 2).
		AllSwapsStopped,
		/// Asset is not an approved external asset.
		UnsupportedAsset,
		/// No PSM instance is registered for the given internal asset.
		PsmNotFound,
		/// Asset is already in the approved list.
		AssetAlreadyApproved,
		/// Asset does not exist.
		AssetDoesNotExist,
		/// Cannot remove asset: not in approved list.
		AssetNotApproved,
		/// Cannot remove asset: has non-zero PSM debt.
		AssetHasDebt,
		/// Operation requires the instance's `full_admin` (Full level); the caller only
		/// matched the `emergency_admin` (Emergency level).
		InsufficientPrivilege,
		/// Maximum number of approved external assets reached.
		TooManyAssets,
		/// Reserved legacy error; retained to preserve error variant indices.
		DecimalsMismatch,
		/// The asset's decimal precision is outside the supported range.
		DecimalsRangeExceeded,
		/// Decimal scaling produced an arithmetic overflow.
		ConversionOverflow,
		/// Conversion to the counter-asset rounds to zero; swap would transfer nothing.
		AmountTooSmallAfterConversion,
		/// A PSM is already registered for this internal asset.
		PsmAlreadyExists,
		/// The PSM has non-zero outstanding debt on at least one approved external.
		PsmHasDebt,
		/// The PSM still has approved externals; remove them before removing the PSM.
		PsmHasApprovedExternals,
		/// An unexpected invariant violation occurred. This should be reported.
		Unexpected,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Swap external asset for internal on a specific PSM instance.
		///
		/// ## Dispatch Origin
		///
		/// Must be `Signed` by the user performing the swap.
		///
		/// ## Details
		///
		/// Transfers `external_amount` of `external_asset` from the caller to the
		/// `internal_asset`'s PSM reserve account, then mints `internal_asset` to the
		/// caller minus the minting fee. The fee is calculated using ceiling rounding
		/// (`mul_ceil`), ensuring the protocol never undercharges. The fee is
		/// transferred to [`PsmInfo::fee_destination`] of the targeted instance.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The internal stablecoin that identifies the PSM instance.
		/// - `external_asset`: The external asset to deposit (must be approved on
		///   `internal_asset`).
		/// - `external_amount`: Amount of external asset to deposit.
		/// - `max_fee`: Maximum minting fee rate accepted by the caller.
		///
		/// ## Errors
		///
		/// - [`Error::PsmNotFound`]: If no PSM is registered for `internal_asset`.
		/// - [`Error::UnsupportedAsset`]: If `external_asset` is not approved on this PSM.
		/// - [`Error::MintingStopped`]: If the per-external circuit breaker is at `MintingDisabled`
		///   or higher.
		/// - [`Error::BelowMinimumSwap`]: If the internal-equivalent of `external_amount` is below
		///   the instance's `min_swap_amount`.
		/// - [`Error::FeeTooHigh`]: If the configured minting fee exceeds `max_fee`.
		/// - [`Error::ExceedsMaxPsmDebt`]: If minting would exceed this PSM's debt ceiling
		///   (aggregate or per-asset).
		/// - [`Error::AmountTooSmallAfterConversion`]: If the conversion to the counter-asset
		///   rounds to zero; swap would transfer nothing.
		///
		/// ## Events
		///
		/// - [`Event::Minted`]: Emitted on successful mint.
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::mint(T::MaxExternals::get()))]
		pub fn mint(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			external_amount: BalanceOf<T>,
			max_fee: Permill,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let info = Psm::<T>::get(&internal_asset).ok_or(Error::<T>::PsmNotFound)?;

			let external = ExternalAssets::<T>::get(&internal_asset, &external_asset)
				.ok_or(Error::<T>::UnsupportedAsset)?;
			ensure!(external.status.allows_minting(), Error::<T>::MintingStopped);

			let ext_decimals = external.decimals;
			let internal_decimals = info.internal_decimals;

			let internal_equivalent =
				Self::external_to_internal(external_amount, ext_decimals, internal_decimals)?;
			ensure!(!internal_equivalent.is_zero(), Error::<T>::AmountTooSmallAfterConversion);
			ensure!(internal_equivalent >= info.min_swap_amount, Error::<T>::BelowMinimumSwap);

			let effective_external =
				Self::internal_to_external(internal_equivalent, ext_decimals, internal_decimals)?;

			let fee_rate = MintingFee::<T>::get(&internal_asset, &external_asset);
			ensure!(fee_rate <= max_fee, Error::<T>::FeeTooHigh);
			let fee = fee_rate.mul_ceil(internal_equivalent);
			let internal_to_user = internal_equivalent.saturating_sub(fee);

			let current_total_psm_debt = Self::total_psm_debt(&internal_asset);
			ensure!(
				current_total_psm_debt.saturating_add(internal_equivalent) <= info.max_debt,
				Error::<T>::ExceedsMaxPsmDebt
			);

			let current_debt = PsmDebt::<T>::get(&internal_asset, &external_asset);
			let max_debt = Self::max_asset_debt(&internal_asset, &external_asset, &info);
			let new_debt = current_debt.saturating_add(internal_equivalent);
			ensure!(new_debt <= max_debt, Error::<T>::ExceedsMaxPsmDebt);

			let psm_account = Self::psm_account(&internal_asset);
			T::Fungibles::transfer(
				external_asset.clone(),
				&who,
				&psm_account,
				effective_external,
				Preservation::Expendable,
			)?;
			T::Fungibles::mint_into(internal_asset.clone(), &who, internal_to_user)?;
			if !fee.is_zero() {
				T::Fungibles::mint_into(internal_asset.clone(), &info.fee_destination, fee)?;
			}

			PsmDebt::<T>::insert(&internal_asset, &external_asset, new_debt);

			Self::deposit_event(Event::Minted {
				who,
				internal_asset,
				external_asset,
				external_consumed: effective_external,
				internal_received: internal_to_user,
				internal_fee: fee,
			});
			Ok(())
		}

		/// Swap internal for external asset on a specific PSM instance.
		///
		/// ## Dispatch Origin
		///
		/// Must be `Signed` by the user performing the swap.
		///
		/// ## Details
		///
		/// Burns `internal_amount` of `internal_asset` from the caller minus fee (transferred
		/// to the instance's [`PsmInfo::fee_destination`]), then transfers the resulting
		/// amount in `external_asset` from the PSM reserve to the caller. The fee is
		/// calculated using ceiling rounding (`mul_ceil`), ensuring the protocol never
		/// undercharges. Redemptions use the decimals snapshotted when the PSM/external pair
		/// was registered, allowing existing positions to unwind even if live metadata later
		/// changes.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The internal stablecoin that identifies the PSM instance.
		/// - `external_asset`: The external asset to receive (must be approved on
		///   `internal_asset`).
		/// - `internal_amount`: Amount of `internal_asset` to redeem.
		/// - `max_fee`: Maximum redemption fee rate accepted by the caller.
		///
		/// ## Errors
		///
		/// - [`Error::PsmNotFound`]: If no PSM is registered for `internal_asset`.
		/// - [`Error::UnsupportedAsset`]: If `external_asset` is not approved on this PSM.
		/// - [`Error::AllSwapsStopped`]: If the per-external circuit breaker is at `AllDisabled`.
		/// - [`Error::BelowMinimumSwap`]: If `internal_amount` is below the instance's
		///   `min_swap_amount`.
		/// - [`Error::FeeTooHigh`]: If the configured redemption fee exceeds `max_fee`.
		/// - [`Error::InsufficientReserve`]: If the PSM holds less of `external_asset` than the
		///   redemption requires.
		/// - [`Error::AmountTooSmallAfterConversion`]: If the conversion to the counter-asset
		///   rounds to zero; swap would transfer nothing.
		///
		/// ## Events
		///
		/// - [`Event::Redeemed`]: Emitted on successful redemption.
		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::redeem())]
		pub fn redeem(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			internal_amount: BalanceOf<T>,
			max_fee: Permill,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let info = Psm::<T>::get(&internal_asset).ok_or(Error::<T>::PsmNotFound)?;

			let external = ExternalAssets::<T>::get(&internal_asset, &external_asset)
				.ok_or(Error::<T>::UnsupportedAsset)?;
			ensure!(external.status.allows_redemption(), Error::<T>::AllSwapsStopped);

			let ext_decimals = external.decimals;
			let internal_decimals = info.internal_decimals;

			ensure!(internal_amount >= info.min_swap_amount, Error::<T>::BelowMinimumSwap);

			let fee_rate = RedemptionFee::<T>::get(&internal_asset, &external_asset);
			ensure!(fee_rate <= max_fee, Error::<T>::FeeTooHigh);
			let mut fee = fee_rate.mul_ceil(internal_amount);
			let internal_net = internal_amount.saturating_sub(fee);

			let external_out =
				Self::internal_to_external(internal_net, ext_decimals, internal_decimals)?;
			ensure!(
				internal_net.is_zero() || !external_out.is_zero(),
				Error::<T>::AmountTooSmallAfterConversion
			);
			// `effective_internal_net` is the internal value that round-trips to `external_out`;
			// it is what we actually burn and what the tracked debt decreases by. Any truncation
			// dust stays in the caller's internal balance, symmetric with `mint`, which takes
			// only the round-tripped share of the external amount.
			let effective_internal_net =
				Self::external_to_internal(external_out, ext_decimals, internal_decimals)?;

			let current_debt = PsmDebt::<T>::get(&internal_asset, &external_asset);
			ensure!(current_debt >= effective_internal_net, Error::<T>::InsufficientReserve);

			let reserve = Self::get_reserve(&internal_asset, &external_asset);
			if reserve < external_out {
				defensive!("PSM reserve is less than expected output amount");
				return Err(Error::<T>::Unexpected.into());
			}

			// `burn_from` would also burn a remainder below `min_balance`, which this pair's debt
			// may not cover. Send it to the fee destination along with the fee instead.
			let remainder = T::Fungibles::balance(internal_asset.clone(), &who)
				.saturating_sub(fee.saturating_add(effective_internal_net));

			if remainder < T::Fungibles::minimum_balance(internal_asset.clone()) {
				fee.saturating_accrue(remainder);
			}

			if !fee.is_zero() {
				T::Fungibles::transfer(
					internal_asset.clone(),
					&who,
					&info.fee_destination,
					fee,
					Preservation::Expendable,
				)?;
			}

			let burned = if !effective_internal_net.is_zero() {
				T::Fungibles::burn_from(
					internal_asset.clone(),
					&who,
					effective_internal_net,
					Preservation::Expendable,
					Precision::Exact,
					Fortitude::Polite,
				)?
			} else {
				BalanceOf::<T>::zero()
			};

			let psm_account = Self::psm_account(&internal_asset);
			if !external_out.is_zero() {
				T::Fungibles::transfer(
					external_asset.clone(),
					&psm_account,
					&who,
					external_out,
					Preservation::Expendable,
				)?;
			}

			PsmDebt::<T>::mutate(&internal_asset, &external_asset, |debt| {
				*debt = debt.saturating_sub(burned);
			});

			Self::deposit_event(Event::Redeemed {
				who,
				internal_asset,
				external_asset,
				internal_consumed: burned.saturating_add(fee),
				external_received: external_out,
				internal_fee: fee,
			});
			Ok(())
		}

		/// Create a PSM for a given internal asset.
		///
		/// If [`Config::CreateOrigin`] resolves to `Some(account)`, takes a
		/// [`Config::Consideration`] deposit from that account for the instance's footprint
		/// (refunded on `remove_psm`). If it resolves to `None`, no deposit is taken. The
		/// `full_admin` and `emergency_admin` origins are set from the provided arguments and may
		/// later be reassigned via [`Pallet::set_full_admin`] / [`Pallet::set_emergency_admin`].
		///
		/// ## Dispatch Origin
		///
		/// [`Config::CreateOrigin`], parameterised by `internal_asset`. With the recommended
		/// [`EnsureAssetOwner`] this is a signed origin that owns `internal_asset`.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The internal stablecoin keying the new PSM. Must exist in the
		///   fungibles backend; must not already have a PSM registered.
		/// - `full_admin`: Origin granted full management of the new PSM.
		/// - `emergency_admin`: Origin granted emergency management of the new PSM.
		/// - `fee_destination`: Account that will receive mint/redeem fees.
		/// - `max_debt`: Initial absolute internal-asset debt ceiling.
		/// - `min_swap_amount`: Minimum swap amount for this instance, in internal-asset units.
		///   Must be non-zero.
		///
		/// ## Errors
		///
		/// - [`DispatchError::BadOrigin`]: The origin is not permitted by [`Config::CreateOrigin`].
		/// - [`Error::PsmAlreadyExists`]: A PSM is already registered for `internal_asset`.
		/// - [`Error::ZeroMinSwapAmount`]: `min_swap_amount` is zero.
		/// - [`Error::AssetDoesNotExist`]: The internal asset does not exist.
		/// - Any error from establishing the [`Config::Consideration`] deposit when one is needed
		///   (e.g. the account cannot afford it).
		///
		/// ## Events
		///
		/// - [`Event::PsmCreated`].
		#[pallet::call_index(2)]
		#[pallet::weight(T::WeightInfo::create_psm())]
		pub fn create_psm(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			full_admin: Box<T::PalletsOrigin>,
			emergency_admin: Box<T::PalletsOrigin>,
			fee_destination: T::AccountId,
			max_debt: BalanceOf<T>,
			min_swap_amount: BalanceOf<T>,
		) -> DispatchResult {
			let maybe_depositor = T::CreateOrigin::ensure_origin(origin, &internal_asset)?;
			ensure!(!Psm::<T>::contains_key(&internal_asset), Error::<T>::PsmAlreadyExists);
			ensure!(!min_swap_amount.is_zero(), Error::<T>::ZeroMinSwapAmount);
			ensure!(
				T::Fungibles::asset_exists(internal_asset.clone()),
				Error::<T>::AssetDoesNotExist
			);

			let deposit = maybe_depositor
				.map(|depositor| {
					T::Consideration::new(&depositor, Self::psm_creation_footprint())
						.map(|ticket| (depositor, ticket))
				})
				.transpose()?;

			let full_admin = *full_admin;
			let emergency_admin = *emergency_admin;
			let internal_decimals = T::Fungibles::decimals(internal_asset.clone());
			Psm::<T>::insert(
				&internal_asset,
				PsmInfo::<T> {
					fee_destination: fee_destination.clone(),
					max_debt,
					min_swap_amount,
					internal_decimals,
					external_count: 0,
				},
			);
			PsmAdmin::<T>::insert(
				&internal_asset,
				PsmAdminInfo::<T> {
					full_admin: full_admin.clone(),
					emergency_admin: emergency_admin.clone(),
					deposit,
				},
			);
			// Acquire a provider reference on the reserve account and the fee destination for the
			// lifetime of this PSM, so they can hold non-sufficient assets (external collateral /
			// minted fees). Released in `remove_psm`. Unconditional (rather than
			// `ensure_account_exists`) so the inc/dec is symmetric even when an account already
			// exists or is shared across PSMs.
			frame_system::Pallet::<T>::inc_providers(&Self::psm_account(&internal_asset));
			frame_system::Pallet::<T>::inc_providers(&fee_destination);

			Self::deposit_event(Event::PsmCreated {
				internal_asset,
				full_admin: Box::new(full_admin),
				emergency_admin: Box::new(emergency_admin),
				fee_destination,
				max_debt,
			});
			Ok(())
		}

		/// Remove a PSM. Callable by the current `full_admin`. All approved externals
		/// must be removed first and aggregate PSM debt must be zero.
		///
		/// If a creation deposit was taken, it is always returned to the account that originally
		/// paid it, regardless of any later admin reassignment.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM's `full_admin`.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to remove.
		///
		/// ## Errors
		///
		/// - [`Error::PsmNotFound`]: No PSM is registered for `internal_asset`.
		/// - [`Error::PsmHasApprovedExternals`]: Approved externals still exist.
		/// - [`Error::PsmHasDebt`]: Outstanding aggregate debt is non-zero.
		///
		/// ## Events
		///
		/// - [`Event::PsmRemoved`].
		#[pallet::call_index(3)]
		#[pallet::weight(T::WeightInfo::remove_psm())]
		pub fn remove_psm(origin: OriginFor<T>, internal_asset: T::AssetId) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_remove_psm())?;
			let info = Psm::<T>::get(&internal_asset).ok_or(Error::<T>::PsmNotFound)?;
			ensure!(info.external_count == 0, Error::<T>::PsmHasApprovedExternals);
			ensure!(Self::total_psm_debt(&internal_asset).is_zero(), Error::<T>::PsmHasDebt);

			let PsmAdminInfo { deposit, .. } =
				PsmAdmin::<T>::get(&internal_asset).ok_or(Error::<T>::PsmNotFound)?;
			if let Some((depositor, ticket)) = deposit {
				ticket.drop(&depositor)?;
			}

			Psm::<T>::remove(&internal_asset);
			PsmAdmin::<T>::remove(&internal_asset);

			// Release the provider references acquired in `create_psm`. Reaps each account when
			// empty; a `ConsumerRemaining` error just means it still holds funds and must stay
			// alive, so the result is intentionally discarded.
			frame_system::Pallet::<T>::dec_providers(&Self::psm_account(&internal_asset)).ok();
			frame_system::Pallet::<T>::dec_providers(&info.fee_destination).ok();

			Self::deposit_event(Event::PsmRemoved { internal_asset });
			Ok(())
		}

		/// Set the minting fee for an `(internal_asset, external_asset)` pair.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin` (the `Full` privilege level).
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to configure.
		/// - `external_asset`: The external asset whose minting fee is being updated.
		/// - `fee`: The new minting fee.
		///
		/// ## Errors
		///
		/// - [`Error::InsufficientPrivilege`]: If the origin only has `Emergency` privileges.
		/// - [`Error::AssetNotApproved`]: If `external_asset` is not approved on `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::MintingFeeUpdated`]: Emitted with old and new values.
		#[pallet::call_index(4)]
		#[pallet::weight(T::WeightInfo::set_minting_fee())]
		pub fn set_minting_fee(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			fee: Permill,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_set_fees())?;
			ensure!(
				ExternalAssets::<T>::contains_key(&internal_asset, &external_asset),
				Error::<T>::AssetNotApproved
			);
			let old_value = MintingFee::<T>::get(&internal_asset, &external_asset);
			MintingFee::<T>::insert(&internal_asset, &external_asset, fee);
			Self::deposit_event(Event::MintingFeeUpdated {
				internal_asset,
				external_asset,
				old_value,
				new_value: fee,
			});
			Ok(())
		}

		/// Set the redemption fee for an `(internal_asset, external_asset)` pair.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin` (the `Full` privilege level).
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to configure.
		/// - `external_asset`: The external asset whose redemption fee is being updated.
		/// - `fee`: The new redemption fee.
		///
		/// ## Errors
		///
		/// - [`Error::InsufficientPrivilege`]: If the origin only has `Emergency` privileges.
		/// - [`Error::AssetNotApproved`]: If `external_asset` is not approved on `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::RedemptionFeeUpdated`]: Emitted with old and new values.
		#[pallet::call_index(5)]
		#[pallet::weight(T::WeightInfo::set_redemption_fee())]
		pub fn set_redemption_fee(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			fee: Permill,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_set_fees())?;
			ensure!(
				ExternalAssets::<T>::contains_key(&internal_asset, &external_asset),
				Error::<T>::AssetNotApproved
			);
			let old_value = RedemptionFee::<T>::get(&internal_asset, &external_asset);
			RedemptionFee::<T>::insert(&internal_asset, &external_asset, fee);
			Self::deposit_event(Event::RedemptionFeeUpdated {
				internal_asset,
				external_asset,
				old_value,
				new_value: fee,
			});
			Ok(())
		}

		/// Set the PSM debt ceiling per internal asset, shared across all approved external
		/// assets.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin`; only the `Full` privilege level may use
		/// this call.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to configure.
		/// - `value`: The new absolute debt ceiling, in internal-asset units.
		///
		/// ## Errors
		///
		/// - [`Error::InsufficientPrivilege`]: If the origin level cannot set the debt ceiling.
		/// - [`Error::PsmNotFound`]: If no PSM is registered for `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::MaxDebtUpdated`]: Emitted with old and new values.
		#[pallet::call_index(6)]
		#[pallet::weight(T::WeightInfo::set_max_debt())]
		pub fn set_max_debt(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			value: BalanceOf<T>,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_set_max_debt())?;
			// `max_debt` only gates minting: lowering it below outstanding debt does not claw
			// anything back, it just pauses minting until redemptions bring the debt back under
			// the new ceiling.
			let old_value =
				Psm::<T>::try_mutate(&internal_asset, |maybe| -> Result<_, DispatchError> {
					let info = maybe.as_mut().ok_or(Error::<T>::PsmNotFound)?;
					Ok(core::mem::replace(&mut info.max_debt, value))
				})?;
			Self::deposit_event(Event::MaxDebtUpdated {
				internal_asset,
				old_value,
				new_value: value,
			});
			Ok(())
		}

		/// Set the circuit breaker per external asset on a PSM instance.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin` or `emergency_admin`; either the
		/// `Full` or `Emergency` privilege level may use this call.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to configure.
		/// - `external_asset`: The external asset whose status is being updated.
		/// - `status`: The new circuit breaker level for that external.
		///
		/// ## Errors
		///
		/// - [`Error::AssetNotApproved`]: If `external_asset` is not approved on `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::AssetStatusUpdated`]: Emitted on a successful update.
		#[pallet::call_index(7)]
		#[pallet::weight(T::WeightInfo::set_asset_status())]
		pub fn set_asset_status(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			status: CircuitBreakerLevel,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_set_circuit_breaker())?;
			ExternalAssets::<T>::try_mutate(
				&internal_asset,
				&external_asset,
				|maybe| -> DispatchResult {
					let info = maybe.as_mut().ok_or(Error::<T>::AssetNotApproved)?;
					info.status = status;
					Ok(())
				},
			)?;
			Self::deposit_event(Event::AssetStatusUpdated {
				internal_asset,
				external_asset,
				status,
			});
			Ok(())
		}

		/// Set the ceiling weight per external asset on a PSM instance.
		///
		/// Weights are normalised against the sum of weights within the same instance:
		/// `max_asset_debt = (weight / sum_of_weights) * info.max_debt`.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin`; only the `Full` privilege level may use
		/// this call.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to configure.
		/// - `external_asset`: The external asset whose ceiling weight is being updated.
		/// - `weight`: The new ceiling weight. Zero disables minting for this external.
		///
		/// ## Errors
		///
		/// - [`Error::InsufficientPrivilege`]: If the origin level cannot set ceiling weights.
		/// - [`Error::AssetNotApproved`]: If `external_asset` is not approved on `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::AssetCeilingWeightUpdated`]: Emitted with old and new values.
		#[pallet::call_index(8)]
		#[pallet::weight(T::WeightInfo::set_asset_ceiling_weight())]
		pub fn set_asset_ceiling_weight(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
			weight: Permill,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_set_asset_ceiling())?;
			ensure!(
				ExternalAssets::<T>::contains_key(&internal_asset, &external_asset),
				Error::<T>::AssetNotApproved
			);
			// Reweighting renormalises every external's ceiling; an external left below its new
			// ceiling simply can't be minted until redemptions bring its debt back down.
			let old_value = AssetCeilingWeight::<T>::get(&internal_asset, &external_asset);
			AssetCeilingWeight::<T>::insert(&internal_asset, &external_asset, weight);
			Self::deposit_event(Event::AssetCeilingWeightUpdated {
				internal_asset,
				external_asset,
				old_value,
				new_value: weight,
			});
			Ok(())
		}

		/// Approve an external asset for a given internal asset.
		///
		/// Snapshots the external asset's live decimals at registration time and
		/// increments [`PsmInfo::external_count`].
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin` (the `Full` privilege level).
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to approve the external on.
		/// - `external_asset`: The external asset to approve.
		///
		/// ## Errors
		///
		/// - [`Error::InsufficientPrivilege`]: If the origin only has `Emergency` privileges.
		/// - [`Error::PsmNotFound`]: If no PSM is registered for `internal_asset`.
		/// - [`Error::TooManyAssets`]: If the PSM is already at [`Config::MaxExternals`].
		/// - [`Error::AssetAlreadyApproved`]: If `external_asset` is already approved on this PSM.
		/// - [`Error::AssetDoesNotExist`]: If `external_asset` does not exist in the underlying
		///   fungibles backend.
		/// - [`Error::DecimalsRangeExceeded`]: If `|asset_decimals − internal_decimals|` exceeds
		///   [`MAX_DECIMALS_DIFF`].
		///
		/// ## Events
		///
		/// - [`Event::ExternalAssetAdded`]: Emitted on a successful approval.
		#[pallet::call_index(9)]
		#[pallet::weight(T::WeightInfo::add_external_asset())]
		pub fn add_external_asset(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_manage_assets())?;
			let mut info = Psm::<T>::get(&internal_asset).ok_or(Error::<T>::PsmNotFound)?;
			ensure!(
				!ExternalAssets::<T>::contains_key(&internal_asset, &external_asset),
				Error::<T>::AssetAlreadyApproved
			);
			ensure!(info.external_count < T::MaxExternals::get(), Error::<T>::TooManyAssets);
			ensure!(
				T::Fungibles::asset_exists(external_asset.clone()),
				Error::<T>::AssetDoesNotExist
			);

			let asset_decimals = T::Fungibles::decimals(external_asset.clone());
			ensure!(
				(asset_decimals.abs_diff(info.internal_decimals) as u32) <= MAX_DECIMALS_DIFF,
				Error::<T>::DecimalsRangeExceeded
			);

			ExternalAssets::<T>::insert(
				&internal_asset,
				&external_asset,
				ExternalAssetInfo {
					status: CircuitBreakerLevel::AllEnabled,
					decimals: asset_decimals,
				},
			);
			info.external_count = info.external_count.saturating_add(1);
			Psm::<T>::insert(&internal_asset, info);

			Self::deposit_event(Event::ExternalAssetAdded { internal_asset, external_asset });
			Ok(())
		}

		/// Remove an external asset from a PSM instance.
		///
		/// Wipes the external's per-instance state (status, decimals, fees, ceiling
		/// weight, debt counter) and decrements [`PsmInfo::external_count`]. The
		/// external must have zero outstanding debt on this instance.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM instance's `full_admin` (the `Full` privilege level).
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM instance to remove the external from.
		/// - `external_asset`: The external asset to remove.
		///
		/// ## Errors
		///
		/// - [`Error::InsufficientPrivilege`]: If the origin only has `Emergency` privileges.
		/// - [`Error::PsmNotFound`]: If no PSM is registered for `internal_asset`.
		/// - [`Error::AssetNotApproved`]: If `external_asset` is not approved on this PSM.
		/// - [`Error::AssetHasDebt`]: If the external still has non-zero outstanding debt.
		///
		/// ## Events
		///
		/// - [`Event::ExternalAssetRemoved`]: Emitted on a successful removal.
		#[pallet::call_index(10)]
		#[pallet::weight(T::WeightInfo::remove_external_asset())]
		pub fn remove_external_asset(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			external_asset: T::AssetId,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_manage_assets())?;
			let mut info = Psm::<T>::get(&internal_asset).ok_or(Error::<T>::PsmNotFound)?;
			ensure!(
				ExternalAssets::<T>::contains_key(&internal_asset, &external_asset),
				Error::<T>::AssetNotApproved
			);
			ensure!(
				PsmDebt::<T>::get(&internal_asset, &external_asset).is_zero(),
				Error::<T>::AssetHasDebt
			);
			ExternalAssets::<T>::remove(&internal_asset, &external_asset);
			MintingFee::<T>::remove(&internal_asset, &external_asset);
			RedemptionFee::<T>::remove(&internal_asset, &external_asset);
			AssetCeilingWeight::<T>::remove(&internal_asset, &external_asset);
			PsmDebt::<T>::remove(&internal_asset, &external_asset);
			info.external_count = info.external_count.saturating_sub(1);
			Psm::<T>::insert(&internal_asset, info);

			Self::deposit_event(Event::ExternalAssetRemoved { internal_asset, external_asset });
			Ok(())
		}

		/// Reassign the PSM's `full_admin`. Callable by the current `full_admin`.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM's current `full_admin`.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM whose `full_admin` is being changed.
		/// - `new_admin`: The new `full_admin` origin.
		///
		/// ## Errors
		///
		/// - [`Error::PsmNotFound`]: No PSM is registered for `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::FullAdminChanged`].
		#[pallet::call_index(11)]
		#[pallet::weight(T::WeightInfo::set_full_admin())]
		pub fn set_full_admin(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			new_admin: Box<T::PalletsOrigin>,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_manage_admins())?;
			let new_admin = *new_admin;
			let old_admin = PsmAdmin::<T>::try_mutate(
				&internal_asset,
				|maybe| -> Result<T::PalletsOrigin, DispatchError> {
					let admin = maybe.as_mut().ok_or(Error::<T>::PsmNotFound)?;
					let old = core::mem::replace(&mut admin.full_admin, new_admin.clone());
					Ok(old)
				},
			)?;
			Self::deposit_event(Event::FullAdminChanged {
				internal_asset,
				old_admin: Box::new(old_admin),
				new_admin: Box::new(new_admin),
			});
			Ok(())
		}

		/// Reassign the PSM's `emergency_admin`. Callable by the current `full_admin`.
		///
		/// ## Dispatch Origin
		///
		/// Must match the PSM's current `full_admin`.
		///
		/// ## Parameters
		///
		/// - `internal_asset`: The PSM whose `emergency_admin` is being changed.
		/// - `new_admin`: The new `emergency_admin` origin.
		///
		/// ## Errors
		///
		/// - [`Error::PsmNotFound`]: No PSM is registered for `internal_asset`.
		///
		/// ## Events
		///
		/// - [`Event::EmergencyAdminChanged`].
		#[pallet::call_index(12)]
		#[pallet::weight(T::WeightInfo::set_emergency_admin())]
		pub fn set_emergency_admin(
			origin: OriginFor<T>,
			internal_asset: T::AssetId,
			new_admin: Box<T::PalletsOrigin>,
		) -> DispatchResult {
			Self::ensure_psm_admin(origin, &internal_asset, |l| l.can_manage_admins())?;
			let new_admin = *new_admin;
			let old_admin = PsmAdmin::<T>::try_mutate(
				&internal_asset,
				|maybe| -> Result<T::PalletsOrigin, DispatchError> {
					let admin = maybe.as_mut().ok_or(Error::<T>::PsmNotFound)?;
					let old = core::mem::replace(&mut admin.emergency_admin, new_admin.clone());
					Ok(old)
				},
			)?;
			Self::deposit_event(Event::EmergencyAdminChanged {
				internal_asset,
				old_admin: Box::new(old_admin),
				new_admin: Box::new(new_admin),
			});
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		/// Derive the reserve account for a PSM instance from the full hash of the pallet-id
		/// domain separator, pallet id, and internal asset.
		pub fn psm_account(internal_asset: &T::AssetId) -> T::AccountId {
			let entropy = (<PalletId as TypeId>::TYPE_ID, T::PalletId::get(), internal_asset)
				.using_encoded(sp_io::hashing::blake2_256);
			T::AccountId::decode(&mut TrailingZeroInput::new(entropy.as_ref()))
				.expect("All byte sequences are valid `AccountId`s; qed")
		}

		/// The footprint of a PSM instance as written by [`Pallet::create_psm`]: the [`Psm`]
		/// and [`PsmAdmin`] entries, both keyed by the internal asset id.
		///
		/// Excludes per-external storage added later via [`Pallet::add_external_asset`];
		/// externals are bounded by [`Config::MaxExternals`] and admin-gated rather than
		/// deposit-backed.
		pub fn psm_creation_footprint() -> Footprint {
			Footprint::from_parts(
				2,
				T::AssetId::max_encoded_len()
					.saturating_mul(2)
					.saturating_add(PsmInfo::<T>::max_encoded_len())
					.saturating_add(PsmAdminInfo::<T>::max_encoded_len()),
			)
		}

		/// PSM debt ceiling for an instance, read from the stored [`PsmInfo`]. Returns
		/// zero if no PSM is installed for `internal_asset`.
		#[cfg(test)]
		pub(crate) fn max_psm_debt(internal_asset: &T::AssetId) -> BalanceOf<T> {
			Psm::<T>::get(internal_asset).map(|p| p.max_debt).unwrap_or_default()
		}

		/// Calculate max debt for a specific external on a PSM.
		///
		/// Weights are normalised against the sum of weights within the same instance to
		/// fill the instance's `max_debt` ceiling. Returns zero if the external has no
		/// configured weight or weights sum to zero.
		pub(crate) fn max_asset_debt(
			internal_asset: &T::AssetId,
			external_asset: &T::AssetId,
			info: &PsmInfo<T>,
		) -> BalanceOf<T> {
			let asset_weight = AssetCeilingWeight::<T>::get(internal_asset, external_asset);
			let total_weight = Self::total_ceiling_weight(internal_asset);
			Self::normalised_ceiling(asset_weight, total_weight, info.max_debt)
		}

		/// Sum of the configured ceiling weights across a PSM's approved externals.
		fn total_ceiling_weight(internal_asset: &T::AssetId) -> u32 {
			AssetCeilingWeight::<T>::iter_prefix(internal_asset)
				.map(|(_, w)| w.deconstruct())
				.fold(0u32, |acc, x| acc.saturating_add(x))
		}

		/// A single external's normalised debt ceiling: its share of the total weight applied
		/// to `max_debt`. Zero if the external (or the PSM as a whole) carries no weight.
		fn normalised_ceiling(
			asset_weight: Permill,
			total_weight: u32,
			max_debt: BalanceOf<T>,
		) -> BalanceOf<T> {
			let weight = asset_weight.deconstruct();
			if weight == 0 || total_weight == 0 {
				return BalanceOf::<T>::zero();
			}
			Perbill::from_rational(weight, total_weight).mul_floor(max_debt)
		}

		/// Total internal-asset debt minted through a PSM instance.
		pub(crate) fn total_psm_debt(internal_asset: &T::AssetId) -> BalanceOf<T> {
			PsmDebt::<T>::iter_prefix_values(internal_asset)
				.fold(BalanceOf::<T>::zero(), |acc, debt| acc.saturating_add(debt))
		}

		/// Whether an external is approved on a PSM instance.
		#[cfg(test)]
		pub(crate) fn is_approved_asset(
			internal_asset: &T::AssetId,
			external_asset: &T::AssetId,
		) -> bool {
			ExternalAssets::<T>::contains_key(internal_asset, external_asset)
		}

		/// Balance of an external held by a PSM instance's reserve account.
		pub(crate) fn get_reserve(
			internal_asset: &T::AssetId,
			external_asset: &T::AssetId,
		) -> BalanceOf<T> {
			T::Fungibles::balance(external_asset.clone(), &Self::psm_account(internal_asset))
		}

		/// Convert an amount denominated in external-asset units into internal units.
		///
		/// Scales by `10^(ext_decimals - internal_decimals)` — multiplies up when internal has more
		/// decimals, floor-divides when it has fewer. Returns [`Error::ConversionOverflow`] if
		/// the scaling factor or the product does not fit in the balance type.
		pub(crate) fn external_to_internal(
			amount: BalanceOf<T>,
			ext_decimals: u8,
			internal_decimals: u8,
		) -> Result<BalanceOf<T>, Error<T>> {
			use core::cmp::Ordering::*;
			match ext_decimals.cmp(&internal_decimals) {
				Equal => Ok(amount),
				Less => {
					let diff = (internal_decimals - ext_decimals) as u32;
					let factor = Self::pow10(diff)?;
					amount.checked_mul(&factor).ok_or(Error::<T>::ConversionOverflow)
				},
				Greater => {
					let diff = (ext_decimals - internal_decimals) as u32;
					let factor = Self::pow10(diff)?;
					Ok(amount.checked_div(&factor).unwrap_or_else(BalanceOf::<T>::zero))
				},
			}
		}

		/// Convert an amount denominated in internal units into external-asset units.
		///
		/// Inverse of [`Self::external_to_internal`]. Floor-divides when internal has more
		/// decimals, multiplies up when it has fewer.
		pub(crate) fn internal_to_external(
			amount: BalanceOf<T>,
			ext_decimals: u8,
			internal_decimals: u8,
		) -> Result<BalanceOf<T>, Error<T>> {
			use core::cmp::Ordering::*;
			match ext_decimals.cmp(&internal_decimals) {
				Equal => Ok(amount),
				Less => {
					let diff = (internal_decimals - ext_decimals) as u32;
					let factor = Self::pow10(diff)?;
					Ok(amount.checked_div(&factor).unwrap_or_else(BalanceOf::<T>::zero))
				},
				Greater => {
					let diff = (ext_decimals - internal_decimals) as u32;
					let factor = Self::pow10(diff)?;
					amount.checked_mul(&factor).ok_or(Error::<T>::ConversionOverflow)
				},
			}
		}

		/// Compute `10^exp` as a [`BalanceOf`]. Returns [`Error::ConversionOverflow`] if the result
		/// does not fit in `u128` or in `BalanceOf<T>`.
		fn pow10(exp: u32) -> Result<BalanceOf<T>, Error<T>> {
			let factor_u128 = 10u128.checked_pow(exp).ok_or(Error::<T>::ConversionOverflow)?;
			factor_u128.try_into().map_err(|_| Error::<T>::ConversionOverflow)
		}

		/// Authorise an operation on the PSM keyed by `internal_asset`.
		///
		/// Matches the incoming origin's caller against the PSM's stored
		/// [`PsmAdminInfo::full_admin`] (yielding `Full`) or [`PsmAdminInfo::emergency_admin`]
		/// (yielding `Emergency`). The resolved level is then checked against `required`. No
		/// other authority can manage a PSM.
		pub(crate) fn ensure_psm_admin(
			origin: OriginFor<T>,
			internal_asset: &T::AssetId,
			required: impl Fn(PsmManagerLevel) -> bool,
		) -> DispatchResult {
			let admin = PsmAdmin::<T>::get(internal_asset).ok_or(Error::<T>::PsmNotFound)?;
			let caller = <T as Config>::RuntimeOrigin::from(origin).into_caller();
			let level = if caller == admin.full_admin {
				PsmManagerLevel::Full
			} else if caller == admin.emergency_admin {
				PsmManagerLevel::Emergency
			} else {
				return Err(DispatchError::BadOrigin);
			};
			ensure!(required(level), Error::<T>::InsufficientPrivilege);
			Ok(())
		}

		#[cfg(any(feature = "try-runtime", test))]
		pub(crate) fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
			use sp_runtime::traits::CheckedAdd;

			for (internal_asset, info) in Psm::<T>::iter() {
				// 0. Every PSM has its paired admin record.
				ensure!(
					PsmAdmin::<T>::contains_key(&internal_asset),
					"PSM instance without a paired PsmAdmin record"
				);

				let mut counted = 0u32;
				for (external_asset, external) in ExternalAssets::<T>::iter_prefix(&internal_asset)
				{
					counted = counted.saturating_add(1);

					// 1. Per-external reserve covers tracked debt.
					let debt = PsmDebt::<T>::get(&internal_asset, &external_asset);
					let reserve = Self::get_reserve(&internal_asset, &external_asset);
					let debt_as_external =
						Self::internal_to_external(debt, external.decimals, info.internal_decimals)
							.map_err(|_| "Failed to convert tracked debt to external units")?;
					ensure!(
						reserve >= debt_as_external,
						"PSM reserve is less than tracked debt for an asset"
					);
				}

				// 2. Cached `external_count` matches the iterated externals.
				ensure!(
					info.external_count == counted,
					"PsmInfo.external_count does not match the approved externals"
				);

				// 3. Sum of per-asset debts equals the aggregate helper.
				let mut sum = BalanceOf::<T>::zero();
				for (_, debt) in PsmDebt::<T>::iter_prefix(&internal_asset) {
					sum = sum.checked_add(&debt).ok_or("PSM debt overflow when summing")?;
				}
				ensure!(
					sum == Self::total_psm_debt(&internal_asset),
					"sum of per-asset debts disagrees with total_psm_debt"
				);
			}

			// 5. No orphaned per-asset state outside registered PSMs.
			for (internal_asset, _, _) in ExternalAssets::<T>::iter() {
				ensure!(
					Psm::<T>::contains_key(&internal_asset),
					"Orphaned ExternalAssets row without parent PSM"
				);
			}
			for (internal_asset, _, _) in PsmDebt::<T>::iter() {
				ensure!(
					Psm::<T>::contains_key(&internal_asset),
					"Orphaned PsmDebt row without parent PSM"
				);
			}
			for (internal_asset, _) in PsmAdmin::<T>::iter() {
				ensure!(
					Psm::<T>::contains_key(&internal_asset),
					"Orphaned PsmAdmin row without parent PSM"
				);
			}

			Ok(())
		}
	}
}
