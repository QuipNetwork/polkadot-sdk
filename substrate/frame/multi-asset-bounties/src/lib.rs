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

//! > Made with *Substrate*, for *Polkadot*.
//!
//! [![github]](https://github.com/paritytech/polkadot-sdk/tree/master/substrate/frame/multi-asset-bounties) -
//! [![polkadot]](https://polkadot.com)
//!
//! [polkadot]: https://img.shields.io/badge/polkadot-E6007A?style=for-the-badge&logo=polkadot&logoColor=white
//! [github]: https://img.shields.io/badge/github-8da0cb?style=for-the-badge&labelColor=555555&logo=github
//!
//!
//! # Multi Asset Bounties Pallet ( `pallet-multi-asset-bounties` )
//!
//! ## Bounty
//!
//! A bounty is a reward for completing a specified body of work or achieving a defined set of
//! objectives. The work must be completed for a predefined amount to be paid out. A curator is
//! assigned when the bounty is funded, and is responsible for awarding the bounty once the
//! objectives are met. To support parallel execution and better governance, a bounty can be split
//! into multiple child bounties. Each child bounty represents a smaller task derived from the
//! parent bounty. The parent bounty curator may assign a separate curator to each child bounty at
//! creation time. The curator may be unassigned, resulting in a new curator election. A bounty may
//! be cancelled at any time—unless a payment has already been attempted and is awaiting status
//! confirmation.
//!
//! > NOTE: A parent bounty cannot be closed if it has any active child bounties associated with it.
//!
//! ### Terminology
//!
//! - **Bounty:** A reward for a predefined body of work upon completion. A bounty defines the total
//!   reward and can be subdivided into multiple child bounties. When referenced in the context of
//!   child bounties, it is referred to as *parent bounty*.
//! - **Curator:** An account managing the bounty and assigning a payout address.
//! - **Child Bounty:** A subtask or milestone funded by a parent bounty. It may carry its own
//!   curator, and reward similar to the parent bounty.
//! - **Curator deposit:** The payment in native asset from a candidate willing to curate a funded
//!   bounty. The deposit is returned when/if the bounty is completed.
//! - **Bounty value:** The total amount in a given asset kind that should be paid to the
//!   Beneficiary if the bounty is rewarded.
//! - **Beneficiary:** The account/location to which the total or part of the bounty is assigned to.
//!
//! ### Account derivation
//!
//! Bounty and child-bounty accounts are derived from the funding source [`PalletId`] using the
//! raw-byte prefixes `b"mbt"` (multi-asset bounty) and `b"mcb"` (multi-asset child bounty).
//!
//! ### Example
//!
//! 1. Fund a bounty approved by spend origin of some asset kind with a proposed curator.
#![doc = docify::embed!("src/tests.rs", fund_bounty_works)]
//! 2. Award a bounty to a beneficiary.
#![doc = docify::embed!("src/tests.rs", award_bounty_works)]
//! ## Pallet API
//!
//! See the [`pallet`] module for more information about the interfaces this pallet exposes,
//! including its configuration trait, dispatchables, storage items, events and errors.

#![cfg_attr(not(feature = "std"), no_std)]

mod benchmarking;
mod mock;
mod tests;
pub mod weights;
#[cfg(feature = "runtime-benchmarks")]
pub use benchmarking::ArgumentsFactory;
pub use pallet::*;
pub use weights::WeightInfo;

extern crate alloc;
use alloc::{boxed::Box, collections::btree_map::BTreeMap};
use frame_support::{
	dispatch::{DispatchResult, DispatchResultWithPostInfo},
	dispatch_context::with_context,
	pallet_prelude::*,
	traits::{
		tokens::{
			Balance, ConversionFromAssetBalance, ConversionToAssetBalance, PayWithSource,
			PaymentStatus,
		},
		Consideration, EnsureOrigin, Get, QueryPreimage, StorePreimage,
	},
	PalletId,
};
use frame_system::pallet_prelude::{
	ensure_signed, BlockNumberFor as SystemBlockNumberFor, OriginFor,
};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::{
		AccountIdConversion, BadOrigin, CheckedAdd, Convert, Saturating, StaticLookup, TryConvert,
		Zero,
	},
	Debug, Permill,
};

/// Lookup type for beneficiary addresses.
pub type BeneficiaryLookupOf<T, I> = <<T as Config<I>>::BeneficiaryLookup as StaticLookup>::Source;
/// An index of a bounty. Just a `u32`.
pub type BountyIndex = u32;
/// Lookup type for account addresses.
pub type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;
/// The payment identifier type used by the [`Config::Paymaster`].
pub type PaymentIdOf<T, I = ()> = <<T as crate::Config<I>>::Paymaster as PayWithSource>::Id;
/// Convenience alias for `Bounty`.
pub type BountyOf<T, I> = Bounty<
	<T as frame_system::Config>::AccountId,
	<T as Config<I>>::Balance,
	<T as Config<I>>::AssetKind,
	<T as frame_system::Config>::Hash,
	PaymentIdOf<T, I>,
	<T as Config<I>>::Beneficiary,
>;
/// Convenience alias for `ChildBounty`.
pub type ChildBountyOf<T, I> = ChildBounty<
	<T as frame_system::Config>::AccountId,
	<T as Config<I>>::Balance,
	<T as frame_system::Config>::Hash,
	PaymentIdOf<T, I>,
	<T as Config<I>>::Beneficiary,
>;

/// A funded bounty.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct Bounty<AccountId, Balance, AssetKind, Hash, PaymentId, Beneficiary> {
	/// The kind of asset this bounty is rewarded in.
	pub asset_kind: AssetKind,
	/// The amount that should be paid if the bounty is rewarded, including
	/// beneficiary payout and possible child bounties.
	///
	/// The asset class determined by `asset_kind`.
	pub value: Balance,
	/// The metadata concerning the bounty.
	///
	/// The `Hash` refers to the preimage of the `Preimages` provider which can be a JSON
	/// dump or IPFS hash of a JSON file.
	pub metadata: Hash,
	/// The status of this bounty.
	pub status: BountyStatus<AccountId, PaymentId, Beneficiary>,
}

/// A funded child-bounty.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct ChildBounty<AccountId, Balance, Hash, PaymentId, Beneficiary> {
	/// The parent bounty index of this child-bounty.
	pub parent_bounty: BountyIndex,
	/// The amount that should be paid if the child-bounty is rewarded.
	///
	/// The asset class determined by the parent bounty `asset_kind`.
	pub value: Balance,
	/// The metadata concerning the child-bounty.
	///
	/// The `Hash` refers to the preimage of the `Preimages` provider which can be a JSON
	/// dump or IPFS hash of a JSON file.
	pub metadata: Hash,
	/// The status of this child-bounty.
	pub status: BountyStatus<AccountId, PaymentId, Beneficiary>,
}

/// The status of a child-/bounty proposal.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub enum BountyStatus<AccountId, PaymentId, Beneficiary> {
	/// The child-/bounty funding has been attempted and is waiting to confirm the funds
	/// allocation.
	///
	/// Call `check_status` to confirm whether the funding payment succeeded. If successful, the
	/// child-/bounty transitions to [`BountyStatus::Funded`]. Otherwise, use `retry_payment` to
	/// reinitiate the funding payment.
	FundingAttempted {
		/// The proposed curator of this child-/bounty.
		curator: AccountId,
		/// The funding payment status from the source (e.g. Treasury, parent bounty) to
		/// the child-/bounty account/location.
		payment_status: PaymentState<PaymentId>,
	},
	/// The child-/bounty is funded and waiting for curator to accept role.
	Funded {
		/// The proposed curator of this child-/bounty.
		curator: AccountId,
	},
	/// The child-/bounty previously assigned curator has been unassigned.
	///
	/// It remains funded and is waiting for a curator proposal.
	CuratorUnassigned,
	/// The child-/bounty is active and waiting to be awarded.
	///
	/// During the `Active` state, the curator can call `fund_child_bounty` to create multiple
	/// child bounties.
	Active {
		/// The curator of this child-/bounty.
		curator: AccountId,
	},
	/// The child-/bounty is closed, and the funds are being refunded to the original source (e.g.,
	/// Treasury). Once `check_status` confirms the payment succeeded, the child-/bounty is
	/// finalized and removed from storage. Otherwise, use `retry_payment` to reinitiate the refund
	/// payment.
	RefundAttempted {
		/// The curator of this child-/bounty.
		///
		/// If `None`, it means the child-/bounty curator was unassigned.
		curator: Option<AccountId>,
		/// The refund payment status from the child-/bounty account/location to the source (e.g.
		/// Treasury, parent bounty).
		payment_status: PaymentState<PaymentId>,
	},
	/// The child-/bounty payout to a beneficiary has been attempted.
	///
	/// Call `check_status` to confirm whether the payout payment succeeded. If successful, the
	/// child-/bounty is finalized and removed from storage. Otherwise, use `retry_payment` to
	/// reinitiate the payout payment.
	PayoutAttempted {
		/// The curator of this child-/bounty.
		curator: AccountId,
		/// The beneficiary stash account/location.
		beneficiary: Beneficiary,
		/// The payout payment status from the child-/bounty account/location to the beneficiary.
		payment_status: PaymentState<PaymentId>,
	},
}

/// The state of a single payment.
///
/// When a payment is initiated via `Paymaster::pay`, it begins in the `Pending` state. The
/// `check_status` call updates the payment state and advances the child-/bounty status. The
/// `retry_payment` call can be used to reattempt payments in either `Pending` or `Failed` states.
#[derive(Encode, Decode, Clone, PartialEq, Eq, MaxEncodedLen, Debug, TypeInfo)]
pub enum PaymentState<Id> {
	/// Pending claim.
	Pending,
	/// Payment attempted with a payment identifier.
	Attempted { id: Id },
	/// Payment failed.
	Failed,
	/// Payment succeeded.
	Succeeded,
}
impl<Id: Clone> PaymentState<Id> {
	/// Used to check if payment can be retried.
	pub fn is_pending_or_failed(&self) -> bool {
		matches!(self, PaymentState::Pending | PaymentState::Failed)
	}

	/// If a payment has been initiated, returns its identifier, which is used to check its
	/// status.
	pub fn get_attempt_id(&self) -> Option<Id> {
		match self {
			PaymentState::Attempted { id } => Some(id.clone()),
			_ => None,
		}
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(1);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T, I = ()>(_);

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// The type in which the assets are measured.
		type Balance: Balance;

		/// Origin from which bounties rejections must come.
		type RejectOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The origin required for funding the bounty. The `Success` value is the maximum amount in
		/// a native asset that this origin is allowed to spend at a time.
		type SpendOrigin: EnsureOrigin<Self::RuntimeOrigin, Success = Self::Balance>;

		/// Type parameter representing the asset kinds used to fund, refund and spend from
		/// bounties.
		type AssetKind: Parameter + MaxEncodedLen;

		/// Type parameter used to identify the beneficiaries eligible to receive payments.
		type Beneficiary: Parameter + MaxEncodedLen;

		/// Converting trait to take a source type and convert to [`Self::Beneficiary`].
		type BeneficiaryLookup: StaticLookup<Target = Self::Beneficiary>;

		/// Minimum value for a bounty.
		#[pallet::constant]
		type BountyValueMinimum: Get<Self::Balance>;

		/// Minimum value for a child-bounty.
		#[pallet::constant]
		type ChildBountyValueMinimum: Get<Self::Balance>;

		/// Maximum number of child bounties that can be added to a parent bounty.
		#[pallet::constant]
		type MaxActiveChildBountyCount: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// Converts an `AssetKind` into the funding source account/location.
		///
		/// Used when initiating funding and refund payments to and from a bounty.
		type FundingSource: TryConvert<
			Self::AssetKind,
			<<Self as pallet::Config<I>>::Paymaster as PayWithSource>::Source,
		>;

		/// Converts a bounty index and `AssetKind` into its funding source account/location.
		///
		/// Used when initiating the funding, refund, and payout payments to and from a bounty.
		type BountySource: TryConvert<
			(BountyIndex, Self::AssetKind),
			<<Self as pallet::Config<I>>::Paymaster as PayWithSource>::Source,
		>;

		/// Converts a parent bounty index, child bounty index, and `AssetKind` into the
		/// child-bounty account/location.
		///
		/// Used when initiating the funding, refund, and payout payments to and from a
		/// child-bounty.
		type ChildBountySource: TryConvert<
			(BountyIndex, BountyIndex, Self::AssetKind),
			<<Self as pallet::Config<I>>::Paymaster as PayWithSource>::Source,
		>;

		/// Type for processing payments of [`Self::AssetKind`] from a `Source` in favor of
		/// [`Self::Beneficiary`].
		type Paymaster: PayWithSource<
			Balance = Self::Balance,
			Source = Self::Beneficiary,
			Beneficiary = Self::Beneficiary,
			AssetKind = Self::AssetKind,
		>;

		/// Type for converting the balance of an [`Self::AssetKind`] to the balance of the native
		/// asset, solely for the purpose of asserting the result against the maximum allowed spend
		/// amount of the [`Self::SpendOrigin`].
		///
		/// The conversion from the native asset balance to the balance of an [`Self::AssetKind`] is
		/// used in benchmarks to convert [`Self::BountyValueMinimum`] to the asset kind amount.
		type BalanceConverter: ConversionFromAssetBalance<Self::Balance, Self::AssetKind, Self::Balance>
			+ ConversionToAssetBalance<Self::Balance, Self::AssetKind, Self::Balance>;

		/// The preimage provider used for child-/bounty metadata.
		type Preimages: QueryPreimage<H = Self::Hashing> + StorePreimage;

		/// Means of associating a cost with committing to the curator role, which is incurred by
		/// the child-/bounty curator.
		///
		/// The footprint accounts for the child-/bounty value converted to the native balance
		/// type (using [`Self::BalanceConverter`]). The native balance type corresponds to the
		/// `Success` type returned by [`Self::SpendOrigin`], which represents the maximum
		/// spendable amount. The bounty amount must be converted with [`Self::BalanceConverter`]
		/// before comparison against this maximum. The cost taken from the curator `AccountId`
		/// may vary based on this converted balance.
		type Consideration: Consideration<Self::AccountId, Self::Balance>;

		/// Helper type for benchmarks.
		#[cfg(feature = "runtime-benchmarks")]
		type BenchmarkHelper: benchmarking::ArgumentsFactory<
			Self::AssetKind,
			Self::Beneficiary,
			Self::Balance,
		>;
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// No child-/bounty at that index.
		InvalidIndex,
		/// The reason given is just too big.
		ReasonTooBig,
		/// Invalid child-/bounty value.
		InvalidValue,
		/// The balance of the asset kind is not convertible to the balance of the native asset for
		/// asserting the origin permissions.
		FailedToConvertBalance,
		/// The child-/bounty status is unexpected.
		UnexpectedStatus,
		/// Require child-/bounty curator.
		RequireCurator,
		/// The spend origin is valid but the amount it is allowed to spend is lower than the
		/// requested amount.
		InsufficientPermission,
		/// There was issue with funding the child-/bounty.
		FundingError,
		/// There was issue with refunding the child-/bounty.
		RefundError,
		// There was issue paying out the child-/bounty.
		PayoutError,
		/// Child-/bounty funding has not concluded yet.
		FundingInconclusive,
		/// Child-/bounty refund has not concluded yet.
		RefundInconclusive,
		/// Child-/bounty payout has not concluded yet.
		PayoutInconclusive,
		/// The child-/bounty or funding source account could not be derived from the indexes and
		/// asset kind.
		FailedToConvertSource,
		/// The parent bounty cannot be closed because it has active child bounties.
		HasActiveChildBounty,
		/// Number of child bounties exceeds limit `MaxActiveChildBountyCount`.
		TooManyChildBounties,
		/// The parent bounty value is not enough to add new child-bounty.
		InsufficientBountyValue,
		/// The preimage does not exist.
		PreimageNotExist,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// A new bounty was created and funding has been initiated.
		BountyCreated { index: BountyIndex },
		/// A new child-bounty was created and funding has been initiated.
		ChildBountyCreated { index: BountyIndex, child_index: BountyIndex },
		/// The curator accepted role and child-/bounty became active.
		BountyBecameActive {
			index: BountyIndex,
			child_index: Option<BountyIndex>,
			curator: T::AccountId,
		},
		/// A child-/bounty was awarded to a beneficiary.
		BountyAwarded {
			index: BountyIndex,
			child_index: Option<BountyIndex>,
			beneficiary: T::Beneficiary,
		},
		/// Payout payment to the beneficiary has concluded successfully.
		BountyPayoutProcessed {
			index: BountyIndex,
			child_index: Option<BountyIndex>,
			asset_kind: T::AssetKind,
			value: T::Balance,
			beneficiary: T::Beneficiary,
		},
		/// Funding payment has concluded successfully.
		BountyFundingProcessed { index: BountyIndex, child_index: Option<BountyIndex> },
		/// Refund payment has concluded successfully.
		BountyRefundProcessed { index: BountyIndex, child_index: Option<BountyIndex> },
		/// A child-/bounty was cancelled.
		BountyCanceled { index: BountyIndex, child_index: Option<BountyIndex> },
		/// A child-/bounty curator was unassigned.
		CuratorUnassigned { index: BountyIndex, child_index: Option<BountyIndex> },
		/// A child-/bounty curator was proposed.
		CuratorProposed {
			index: BountyIndex,
			child_index: Option<BountyIndex>,
			curator: T::AccountId,
		},
		/// A payment failed and can be retried.
		PaymentFailed {
			index: BountyIndex,
			child_index: Option<BountyIndex>,
			payment_id: PaymentIdOf<T, I>,
		},
		/// A payment happened and can be checked.
		Paid { index: BountyIndex, child_index: Option<BountyIndex>, payment_id: PaymentIdOf<T, I> },
		/// A bounty's value was increased by its curator.
		BountyValueIncreased { index: BountyIndex, old_value: T::Balance, new_value: T::Balance },
	}

	/// A reason for this pallet placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason<I: 'static = ()> {
		/// The funds are held as deposit for the curator commitment to a bounty.
		#[codec(index = 0)]
		CuratorDeposit,
	}

	/// Number of bounty proposals that have been made.
	#[pallet::storage]
	pub type BountyCount<T: Config<I>, I: 'static = ()> = StorageValue<_, u32, ValueQuery>;

	/// Bounties that have been made.
	#[pallet::storage]
	pub type Bounties<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, BountyIndex, BountyOf<T, I>>;

	/// Child bounties that have been added.
	///
	/// Indexed by `(parent_bounty_id, child_bounty_id)`.
	#[pallet::storage]
	pub type ChildBounties<T: Config<I>, I: 'static = ()> = StorageDoubleMap<
		_,
		Twox64Concat,
		BountyIndex,
		Twox64Concat,
		BountyIndex,
		ChildBountyOf<T, I>,
	>;

	/// Number of active child bounties per parent bounty.
	///
	/// Indexed by `parent_bounty_id`.
	#[pallet::storage]
	pub type ChildBountiesPerParent<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, BountyIndex, u32, ValueQuery>;

	/// Number of total child bounties per parent bounty, including completed bounties.
	///
	/// Indexed by `parent_bounty_id`.
	#[pallet::storage]
	pub type TotalChildBountiesPerParent<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, BountyIndex, u32, ValueQuery>;

	/// The cumulative child-bounty value for each parent bounty. To be subtracted from the parent
	/// bounty payout when awarding bounty.
	///
	/// Indexed by `parent_bounty_id`.
	#[pallet::storage]
	pub type ChildBountiesValuePerParent<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, BountyIndex, T::Balance, ValueQuery>;

	/// The consideration cost incurred by the child-/bounty curator for committing to the role.
	///
	/// Determined by [`pallet::Config::Consideration`]. It is created when the curator accepts the
	/// role, and is either burned if the curator misbehaves or consumed upon successful
	/// completion of the child-/bounty.
	///
	/// Note: If the parent curator is also assigned to the child-bounty,  
	/// the consideration cost is charged only once — when the curator  
	/// accepts the role for the parent bounty.
	///
	/// Indexed by `(parent_bounty_id, child_bounty_id)`.
	#[pallet::storage]
	pub type CuratorDeposit<T: Config<I>, I: 'static = ()> = StorageDoubleMap<
		_,
		Twox64Concat,
		BountyIndex,
		Twox64Concat,
		Option<BountyIndex>,
		T::Consideration,
	>;

	/// Temporarily tracks spending limits within the current context to prevent overspending.
	#[derive(Default)]
	pub struct SpendContext<Balance> {
		pub spend_in_context: BTreeMap<Balance, Balance>,
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Fund a new bounty with a proposed curator, initiating the payment from the
		/// funding source to the bounty account/location.
		///
		/// ## Dispatch Origin
		///
		/// Must be [`Config::SpendOrigin`] with the `Success` value being at least
		/// the bounty value converted to native balance using [`Config::BalanceConverter`].
		/// The converted native amount is validated against the maximum spendable amount
		/// returned by [`Config::SpendOrigin`].
		///
		/// ## Details
		///
		/// - The `SpendOrigin` must have sufficient permissions to fund the bounty.
		/// - The bounty `value` (in asset balance) is converted to native balance for validation.
		/// - In case of a funding failure, the bounty status must be updated with the
		///   `check_status` call before retrying with `retry_payment` call.
		///
		/// ### Parameters
		/// - `asset_kind`: An indicator of the specific asset class to be funded.
		/// - `value`: The total payment amount of this bounty.
		/// - `curator`: Address of bounty curator.
		/// - `metadata`: The hash of an on-chain stored preimage with bounty metadata.
		///
		/// ## Events
		///
		/// Emits [`Event::BountyCreated`] and [`Event::Paid`] if successful.
		#[pallet::call_index(0)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::fund_bounty())]
		pub fn fund_bounty(
			origin: OriginFor<T>,
			asset_kind: Box<T::AssetKind>,
			#[pallet::compact] value: T::Balance,
			curator: AccountIdLookupOf<T>,
			metadata: T::Hash,
		) -> DispatchResult {
			let max_amount = T::SpendOrigin::ensure_origin(origin)?;
			let curator = T::Lookup::lookup(curator)?;
			ensure!(T::Preimages::len(&metadata).is_some(), Error::<T, I>::PreimageNotExist);

			let native_amount = T::BalanceConverter::from_asset_balance(value, *asset_kind.clone())
				.map_err(|_| Error::<T, I>::FailedToConvertBalance)?;
			ensure!(native_amount >= T::BountyValueMinimum::get(), Error::<T, I>::InvalidValue);
			ensure!(native_amount <= max_amount, Error::<T, I>::InsufficientPermission);

			with_context::<SpendContext<T::Balance>, _>(|v| {
				let context = v.or_default();
				let funding = context.spend_in_context.entry(max_amount).or_default();

				if funding.checked_add(&native_amount).map(|s| s > max_amount).unwrap_or(true) {
					Err(Error::<T, I>::InsufficientPermission)
				} else {
					*funding = funding.saturating_add(native_amount);
					Ok(())
				}
			})
			.unwrap_or(Ok(()))?;

			let index = BountyCount::<T, I>::get();
			let payment_status =
				Self::do_process_funding_payment(index, None, *asset_kind.clone(), value, None)?;

			let bounty = BountyOf::<T, I> {
				asset_kind: *asset_kind,
				value,
				metadata,
				status: BountyStatus::FundingAttempted { curator, payment_status },
			};
			Bounties::<T, I>::insert(index, &bounty);
			T::Preimages::request(&metadata);
			BountyCount::<T, I>::put(index + 1);

			Self::deposit_event(Event::<T, I>::BountyCreated { index });

			Ok(())
		}

		/// Fund a new child-bounty with a proposed curator, initiating the payment from the parent
		/// bounty to the child-bounty account/location.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the parent curator.
		///
		/// ## Details
		///
		/// - If `curator` is not provided, the child-bounty will default to using the parent
		///   curator, allowing the parent curator to immediately call `check_status` and
		///   `award_bounty` to payout the child-bounty.
		/// - In case of a funding failure, the child-/bounty status must be updated with the
		///   `check_status` call before retrying with `retry_payment` call.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty for which child-bounty is being added.
		/// - `value`: The payment amount of this child-bounty.
		/// - `metadata`: The hash of an on-chain stored preimage with child-bounty metadata.
		/// - `curator`: Address of child-bounty curator.
		///
		/// ## Events
		///
		/// Emits [`Event::ChildBountyCreated`] and [`Event::Paid`] if successful.
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::fund_child_bounty())]
		pub fn fund_child_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			#[pallet::compact] value: T::Balance,
			metadata: T::Hash,
			curator: Option<AccountIdLookupOf<T>>,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;
			ensure!(T::Preimages::len(&metadata).is_some(), Error::<T, I>::PreimageNotExist);

			let (asset_kind, parent_value, _, _, parent_curator) =
				Self::get_bounty_details(parent_bounty_id, None)
					.map_err(|_| Error::<T, I>::InvalidIndex)?;
			let native_amount = T::BalanceConverter::from_asset_balance(value, asset_kind.clone())
				.map_err(|_| Error::<T, I>::FailedToConvertBalance)?;

			ensure!(
				native_amount >= T::ChildBountyValueMinimum::get(),
				Error::<T, I>::InvalidValue
			);
			ensure!(
				ChildBountiesPerParent::<T, I>::get(parent_bounty_id) <
					T::MaxActiveChildBountyCount::get(),
				Error::<T, I>::TooManyChildBounties,
			);

			// Parent bounty must be `Active` with a curator assigned.
			let parent_curator = parent_curator.ok_or(Error::<T, I>::UnexpectedStatus)?;
			let final_curator = match curator {
				Some(curator) => T::Lookup::lookup(curator)?,
				None => parent_curator.clone(),
			};
			ensure!(signer == parent_curator, Error::<T, I>::RequireCurator);

			// Check value
			let child_bounties_value = ChildBountiesValuePerParent::<T, I>::get(parent_bounty_id);
			let remaining_parent_value = parent_value.saturating_sub(child_bounties_value);
			ensure!(remaining_parent_value >= value, Error::<T, I>::InsufficientBountyValue);

			// Get child-bounty ID.
			let child_bounty_id = TotalChildBountiesPerParent::<T, I>::get(parent_bounty_id);

			// Initiate funding payment
			let payment_status = Self::do_process_funding_payment(
				parent_bounty_id,
				Some(child_bounty_id),
				asset_kind,
				value,
				None,
			)?;

			let child_bounty = ChildBounty {
				parent_bounty: parent_bounty_id,
				value,
				metadata,
				status: BountyStatus::FundingAttempted {
					curator: final_curator,
					payment_status: payment_status.clone(),
				},
			};
			ChildBounties::<T, I>::insert(parent_bounty_id, child_bounty_id, child_bounty);
			T::Preimages::request(&metadata);

			// Add child-bounty value to the cumulative value sum. To be
			// subtracted from the parent bounty payout when awarding
			// bounty.
			ChildBountiesValuePerParent::<T, I>::mutate(parent_bounty_id, |children_value| {
				*children_value = children_value.saturating_add(value)
			});

			// Increment the active child-bounty count.
			ChildBountiesPerParent::<T, I>::mutate(parent_bounty_id, |count| {
				count.saturating_inc()
			});
			TotalChildBountiesPerParent::<T, I>::insert(
				parent_bounty_id,
				child_bounty_id.saturating_add(1),
			);

			Self::deposit_event(Event::<T, I>::ChildBountyCreated {
				index: parent_bounty_id,
				child_index: child_bounty_id,
			});

			Ok(())
		}

		/// Propose a new curator for a child-/bounty after the previous was unassigned.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by `T::SpendOrigin` for a bounty, or by the parent bounty curator
		/// for a child-bounty.
		///
		/// ## Details
		///
		/// - The child-/bounty must be in the `CuratorUnassigned` state.
		/// - For a bounty, the `SpendOrigin` must have sufficient permissions to propose the
		///   curator.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		/// - `curator`: Account to be proposed as the curator.
		///
		/// ## Events
		///
		/// Emits [`Event::CuratorProposed`] if successful.
		#[pallet::call_index(2)]
		#[pallet::weight(match child_bounty_id {
			None => <T as Config<I>>::WeightInfo::propose_curator_parent_bounty(),
			Some(_) => <T as Config<I>>::WeightInfo::propose_curator_child_bounty(),
		})]
		pub fn propose_curator(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
			curator: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let maybe_sender = ensure_signed(origin.clone())
				.map(Some)
				.or_else(|_| T::SpendOrigin::ensure_origin(origin.clone()).map(|_| None))?;
			let curator = T::Lookup::lookup(curator)?;

			let (asset_kind, value, _, status, parent_curator) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;
			ensure!(status == BountyStatus::CuratorUnassigned, Error::<T, I>::UnexpectedStatus);

			match child_bounty_id {
				// Only `SpendOrigin` can propose curator for bounty
				None => {
					ensure!(maybe_sender.is_none(), BadOrigin);
					let max_amount = T::SpendOrigin::ensure_origin(origin)?;
					let native_amount = T::BalanceConverter::from_asset_balance(value, asset_kind)
						.map_err(|_| Error::<T, I>::FailedToConvertBalance)?;
					ensure!(native_amount <= max_amount, Error::<T, I>::InsufficientPermission);
				},
				// Only parent curator can propose curator for child-bounty
				Some(_) => {
					let parent_curator = parent_curator.ok_or(Error::<T, I>::UnexpectedStatus)?;
					let sender = maybe_sender.ok_or(BadOrigin)?;
					ensure!(sender == parent_curator, BadOrigin);
				},
			};

			let new_status = BountyStatus::Funded { curator: curator.clone() };
			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Self::deposit_event(Event::<T, I>::CuratorProposed {
				index: parent_bounty_id,
				child_index: child_bounty_id,
				curator,
			});

			Ok(())
		}

		/// Accept the curator role for a child-/bounty.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the proposed curator.
		///
		/// ## Details
		///
		/// - The child-/bounty must be in the `Funded` state.
		/// - The curator must accept the role by calling this function.
		/// - A deposit will be reserved from the curator and refunded upon successful payout.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		///
		/// ## Events
		///
		/// Emits [`Event::BountyBecameActive`] if successful.
		#[pallet::call_index(3)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::accept_curator())]
		pub fn accept_curator(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;

			let (asset_kind, value, _, status, _) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;

			let BountyStatus::Funded { ref curator } = status else {
				return Err(Error::<T, I>::UnexpectedStatus.into());
			};
			ensure!(signer == *curator, Error::<T, I>::RequireCurator);

			let native_amount = T::BalanceConverter::from_asset_balance(value, asset_kind)
				.map_err(|_| Error::<T, I>::FailedToConvertBalance)?;
			let curator_deposit = T::Consideration::new(&curator, native_amount)?;
			CuratorDeposit::<T, I>::insert(parent_bounty_id, child_bounty_id, curator_deposit);

			let new_status = BountyStatus::Active { curator: curator.clone() };
			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Self::deposit_event(Event::<T, I>::BountyBecameActive {
				index: parent_bounty_id,
				child_index: child_bounty_id,
				curator: signer,
			});

			Ok(())
		}

		/// Unassign curator from a child-/bounty.
		///
		/// ## Dispatch Origin
		///
		/// This function can only be called by the `RejectOrigin` or the child-/bounty curator.
		///
		/// ## Details
		///
		/// - If this function is called by the `RejectOrigin`, or by the parent curator in the case
		///   of a child bounty, we assume that the curator is malicious or inactive. As a result,
		///   we will slash the curator when possible.
		/// - If the origin is the child-/bounty curator, we take this as a sign they are unable to
		///   do their job and they willingly give up. We could slash them, but for now we allow
		///   them to recover their deposit and exit without issue. (We may want to change this if
		///   it is abused).
		/// - If successful, the child-/bounty status is updated to `CuratorUnassigned`. To
		///   reactivate the bounty, a new curator must be proposed and must accept the role.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		///
		/// ## Events
		///
		/// Emits [`Event::CuratorUnassigned`] if successful.
		#[pallet::call_index(4)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::unassign_curator())]
		pub fn unassign_curator(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
		) -> DispatchResult {
			let maybe_sender = ensure_signed(origin.clone())
				.map(Some)
				.or_else(|_| T::RejectOrigin::ensure_origin(origin).map(|_| None))?;

			let (_, _, _, status, parent_curator) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;

			match status {
				BountyStatus::Funded { ref curator } => {
					// A bounty curator has been proposed, but not accepted yet.
					// Either `RejectOrigin`, parent bounty curator or the proposed
					// curator can unassign the child-/bounty curator.
					ensure!(
						maybe_sender.map_or(true, |sender| {
							sender == *curator ||
								parent_curator
									.map_or(false, |parent_curator| sender == parent_curator)
						}),
						BadOrigin
					);
				},
				BountyStatus::Active { ref curator, .. } => {
					// The child-/bounty is active.
					match maybe_sender {
						// If the `RejectOrigin` is calling this function, burn the curator deposit.
						None => {
							if let Some(curator_deposit) =
								CuratorDeposit::<T, I>::take(parent_bounty_id, child_bounty_id)
							{
								T::Consideration::burn(curator_deposit, curator);
							}
							// Continue to change bounty status below...
						},
						Some(sender) if sender == *curator => {
							if let Some(curator_deposit) =
								CuratorDeposit::<T, I>::get(parent_bounty_id, child_bounty_id)
							{
								// This is the curator, willingly giving up their role. Free their
								// deposit.
								T::Consideration::drop(curator_deposit, curator)?;
								CuratorDeposit::<T, I>::remove(parent_bounty_id, child_bounty_id);
							}
							// Continue to change bounty status below...
						},
						Some(sender) => {
							let parent_curator = parent_curator.ok_or(BadOrigin)?;
							ensure!(
								sender == parent_curator && *curator != parent_curator,
								BadOrigin
							);
							// Parent curator is unassigning the child curator. Burn the curator
							// deposit.
							if let Some(curator_deposit) =
								CuratorDeposit::<T, I>::take(parent_bounty_id, child_bounty_id)
							{
								T::Consideration::burn(curator_deposit, curator);
							}
						},
					}
				},
				_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
			};

			let new_status = BountyStatus::CuratorUnassigned;
			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Self::deposit_event(Event::<T, I>::CuratorUnassigned {
				index: parent_bounty_id,
				child_index: child_bounty_id,
			});

			Ok(())
		}

		/// Awards the child-/bounty to a beneficiary account/location,
		/// initiating the payout payments to both the beneficiary and the curator.
		///
		/// ## Dispatch Origin
		///
		/// This function can only be called by the `RejectOrigin` or the child-/bounty curator.
		///
		/// ## Details
		///
		/// - The child-/bounty must be in the `Active` state.
		/// - if awarding a parent bounty it must not have active or funded child bounties.
		/// - Initiates payout payment from the child-/bounty to the beneficiary account/location.
		/// - If successful the child-/bounty status is updated to `PayoutAttempted`.
		/// - In case of a payout failure, the child-/bounty status must be updated with
		/// `check_status` call before retrying with `retry_payment` call.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		/// - `beneficiary`: Account/location to be awarded the child-/bounty.
		///
		/// ## Events
		///
		/// Emits [`Event::BountyAwarded`] and [`Event::Paid`] if successful.
		#[pallet::call_index(5)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::award_bounty())]
		pub fn award_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
			beneficiary: BeneficiaryLookupOf<T, I>,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;
			let beneficiary = T::BeneficiaryLookup::lookup(beneficiary)?;

			let (asset_kind, value, _, status, _) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;

			if child_bounty_id.is_none() {
				ensure!(
					ChildBountiesPerParent::<T, I>::get(parent_bounty_id) == 0,
					Error::<T, I>::HasActiveChildBounty
				);
			}

			let BountyStatus::Active { ref curator } = status else {
				return Err(Error::<T, I>::UnexpectedStatus.into());
			};
			ensure!(signer == *curator, Error::<T, I>::RequireCurator);

			let beneficiary_payment_status = Self::do_process_payout_payment(
				parent_bounty_id,
				child_bounty_id,
				asset_kind,
				value,
				beneficiary.clone(),
				None,
			)?;

			let new_status = BountyStatus::PayoutAttempted {
				curator: curator.clone(),
				beneficiary: beneficiary.clone(),
				payment_status: beneficiary_payment_status.clone(),
			};
			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Self::deposit_event(Event::<T, I>::BountyAwarded {
				index: parent_bounty_id,
				child_index: child_bounty_id,
				beneficiary,
			});

			Ok(())
		}

		/// Cancel an active child-/bounty. A payment to send all the funds to the funding source is
		/// initialized.
		///
		/// ## Dispatch Origin
		///
		/// This function can only be called by the `RejectOrigin` or the parent bounty curator.
		///
		/// ## Details
		///
		/// - If the child-/bounty is in the `Funded` state, a refund payment is initiated.
		/// - If the child-/bounty is in the `Active` state, a refund payment is initiated and the
		///   child-/bounty status is updated with the curator account/location.
		/// - If the child-/bounty is in the funding or payout phase, it cannot be canceled.
		/// - In case of a refund failure, the child-/bounty status must be updated with the
		/// `check_status` call before retrying with `retry_payment` call.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		///
		/// ## Events
		///
		/// Emits [`Event::BountyCanceled`] and [`Event::Paid`] if successful.
		#[pallet::call_index(6)]
		#[pallet::weight(match child_bounty_id {
			None => <T as Config<I>>::WeightInfo::close_parent_bounty(),
			Some(_) => <T as Config<I>>::WeightInfo::close_child_bounty(),
		})]
		pub fn close_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
		) -> DispatchResult {
			let maybe_sender = ensure_signed(origin.clone())
				.map(Some)
				.or_else(|_| T::RejectOrigin::ensure_origin(origin).map(|_| None))?;

			let (asset_kind, value, _, status, parent_curator) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;

			let maybe_curator = match status {
				BountyStatus::Funded { curator } | BountyStatus::Active { curator, .. } => {
					Some(curator)
				},
				BountyStatus::CuratorUnassigned => None,
				_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
			};

			match child_bounty_id {
				None => {
					// Parent bounty can only be closed if it has no active child bounties.
					ensure!(
						ChildBountiesPerParent::<T, I>::get(parent_bounty_id) == 0,
						Error::<T, I>::HasActiveChildBounty
					);
					// Bounty can be closed by `RejectOrigin` or the curator.
					if let Some(sender) = maybe_sender.as_ref() {
						let is_curator =
							maybe_curator.as_ref().map_or(false, |curator| curator == sender);
						ensure!(is_curator, BadOrigin);
					}
				},
				Some(_) => {
					// Child-bounty can be closed by `RejectOrigin`, the curator or parent curator.
					if let Some(sender) = maybe_sender.as_ref() {
						let is_curator =
							maybe_curator.as_ref().map_or(false, |curator| curator == sender);
						let is_parent_curator = parent_curator
							.as_ref()
							.map_or(false, |parent_curator| parent_curator == sender);
						ensure!(is_curator || is_parent_curator, BadOrigin);
					}
				},
			};

			let payment_status = Self::do_process_refund_payment(
				parent_bounty_id,
				child_bounty_id,
				asset_kind,
				value,
				None,
			)?;
			let new_status = BountyStatus::RefundAttempted {
				payment_status: payment_status.clone(),
				curator: maybe_curator.clone(),
			};
			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Self::deposit_event(Event::<T, I>::BountyCanceled {
				index: parent_bounty_id,
				child_index: child_bounty_id,
			});

			Ok(())
		}

		/// Check and update the payment status of a child-/bounty.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed.
		///
		/// ## Details
		///
		/// - If the child-/bounty status is `FundingAttempted`, it checks if the funding payment
		///   has succeeded. If successful, the bounty status becomes `Funded`.
		/// - If the child-/bounty status is `RefundAttempted`, it checks if the refund payment has
		///   succeeded. If successful, the child-/bounty is removed from storage.
		/// - If the child-/bounty status is `PayoutAttempted`, it checks if the payout payment has
		///   succeeded. If successful, the child-/bounty is removed from storage.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		///
		/// ## Events
		///
		/// Emits [`Event::BountyBecameActive`] if the child/bounty status transitions to `Active`.
		/// Emits [`Event::BountyRefundProcessed`] if the refund payment has succeed.
		/// Emits [`Event::BountyPayoutProcessed`] if the payout payment has succeed.
		/// Emits [`Event::PaymentFailed`] if the funding, refund our payment payment has failed.
		#[pallet::call_index(7)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::check_status_funding().max(
			<T as Config<I>>::WeightInfo::check_status_refund(),
		).max(<T as Config<I>>::WeightInfo::check_status_payout()))]
		pub fn check_status(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
		) -> DispatchResultWithPostInfo {
			use BountyStatus::*;

			ensure_signed(origin)?;
			let (asset_kind, value, metadata, status, parent_curator) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;

			let (new_status, weight) = match status {
				FundingAttempted { ref payment_status, curator } => {
					let new_payment_status = Self::do_check_funding_payment_status(
						parent_bounty_id,
						child_bounty_id,
						payment_status.clone(),
					)?;

					let new_status = match new_payment_status {
						PaymentState::Succeeded => match (child_bounty_id, parent_curator) {
							(Some(_), Some(parent_curator)) if curator == parent_curator => {
								BountyStatus::Active { curator }
							},
							_ => BountyStatus::Funded { curator },
						},
						PaymentState::Pending |
						PaymentState::Failed |
						PaymentState::Attempted { .. } => BountyStatus::FundingAttempted {
							payment_status: new_payment_status,
							curator,
						},
					};

					let weight = <T as Config<I>>::WeightInfo::check_status_funding();

					(new_status, weight)
				},
				RefundAttempted { ref payment_status, ref curator } => {
					let new_payment_status = Self::do_check_refund_payment_status(
						parent_bounty_id,
						child_bounty_id,
						payment_status.clone(),
					)?;

					let new_status = match new_payment_status {
						PaymentState::Succeeded => {
							if let Some(curator) = curator {
								// Drop the curator deposit when payment succeeds
								// If the parent curator is also the child curator, there
								// is no deposit
								if let Some(curator_deposit) =
									CuratorDeposit::<T, I>::take(parent_bounty_id, child_bounty_id)
								{
									T::Consideration::drop(curator_deposit, curator)?;
								}
							}
							if let Some(_) = child_bounty_id {
								// Revert the value back to parent bounty
								ChildBountiesValuePerParent::<T, I>::mutate(
									parent_bounty_id,
									|total_value| *total_value = total_value.saturating_sub(value),
								);
							}
							// refund succeeded, cleanup the bounty
							Self::remove_bounty(parent_bounty_id, child_bounty_id, metadata);
							return Ok(Pays::No.into());
						},
						PaymentState::Pending |
						PaymentState::Failed |
						PaymentState::Attempted { .. } => BountyStatus::RefundAttempted {
							payment_status: new_payment_status,
							curator: curator.clone(),
						},
					};

					let weight = <T as Config<I>>::WeightInfo::check_status_refund();

					(new_status, weight)
				},
				PayoutAttempted { ref curator, ref beneficiary, ref payment_status } => {
					let new_payment_status = Self::do_check_payout_payment_status(
						parent_bounty_id,
						child_bounty_id,
						asset_kind,
						value,
						beneficiary.clone(),
						payment_status.clone(),
					)?;

					let new_status = match new_payment_status {
						PaymentState::Succeeded => {
							if let Some(curator_deposit) =
								CuratorDeposit::<T, I>::take(parent_bounty_id, child_bounty_id)
							{
								// Drop the curator deposit when both payments succeed
								// If the child curator is the parent curator, the
								// deposit is 0
								T::Consideration::drop(curator_deposit, curator)?;
							}
							// payout succeeded, cleanup the bounty
							Self::remove_bounty(parent_bounty_id, child_bounty_id, metadata);
							return Ok(Pays::No.into());
						},
						PaymentState::Pending |
						PaymentState::Failed |
						PaymentState::Attempted { .. } => BountyStatus::PayoutAttempted {
							curator: curator.clone(),
							beneficiary: beneficiary.clone(),
							payment_status: new_payment_status.clone(),
						},
					};

					let weight = <T as Config<I>>::WeightInfo::check_status_payout();

					(new_status, weight)
				},
				_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
			};

			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Ok(Some(weight).into())
		}

		/// Retry the funding, refund or payout payments.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed.
		///
		/// ## Details
		///
		/// - If the child-/bounty status is `FundingAttempted`, it retries the funding payment from
		///   funding source the child-/bounty account/location.
		/// - If the child-/bounty status is `RefundAttempted`, it retries the refund payment from
		///   the child-/bounty account/location to the funding source.
		/// - If the child-/bounty status is `PayoutAttempted`, it retries the payout payment from
		///   the child-/bounty account/location to the beneficiary account/location.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of parent bounty.
		/// - `child_bounty_id`: Index of child-bounty.
		///
		/// ## Events
		///
		/// Emits [`Event::Paid`] if the funding, refund or payout payment has initiated.
		#[pallet::call_index(8)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::retry_payment_funding().max(
			<T as Config<I>>::WeightInfo::retry_payment_refund(),
		).max(<T as Config<I>>::WeightInfo::retry_payment_payout()))]
		pub fn retry_payment(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			child_bounty_id: Option<BountyIndex>,
		) -> DispatchResultWithPostInfo {
			use BountyStatus::*;

			ensure_signed(origin)?;
			let (asset_kind, value, _, status, _) =
				Self::get_bounty_details(parent_bounty_id, child_bounty_id)?;

			let (new_status, weight) = match status {
				FundingAttempted { ref payment_status, ref curator } => {
					let new_payment_status = Self::do_process_funding_payment(
						parent_bounty_id,
						child_bounty_id,
						asset_kind,
						value,
						Some(payment_status.clone()),
					)?;

					(
						FundingAttempted {
							payment_status: new_payment_status,
							curator: curator.clone(),
						},
						<T as Config<I>>::WeightInfo::retry_payment_funding(),
					)
				},
				RefundAttempted { ref curator, ref payment_status } => {
					let new_payment_status = Self::do_process_refund_payment(
						parent_bounty_id,
						child_bounty_id,
						asset_kind,
						value,
						Some(payment_status.clone()),
					)?;
					(
						RefundAttempted {
							curator: curator.clone(),
							payment_status: new_payment_status,
						},
						<T as Config<I>>::WeightInfo::retry_payment_refund(),
					)
				},
				PayoutAttempted { ref curator, ref beneficiary, ref payment_status } => {
					let new_payment_status = Self::do_process_payout_payment(
						parent_bounty_id,
						child_bounty_id,
						asset_kind,
						value,
						beneficiary.clone(),
						Some(payment_status.clone()),
					)?;
					(
						PayoutAttempted {
							curator: curator.clone(),
							beneficiary: beneficiary.clone(),
							payment_status: new_payment_status,
						},
						<T as Config<I>>::WeightInfo::retry_payment_payout(),
					)
				},
				_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
			};

			Self::update_bounty_status(parent_bounty_id, child_bounty_id, new_status)?;

			Ok(Some(weight).into())
		}

		/// Increase the value of an active bounty by `amount`.
		///
		/// ## Dispatch Origin
		///
		/// Must be signed by the bounty curator.
		///
		/// ## Details
		///
		/// - The bounty must be in the `Active` state.
		/// - Raises the recorded `value` by `amount`. This is used to register funds that were
		///   transferred into the bounty account out-of-band (e.g. recurring external top-ups), so
		///   they become available to award or to allocate to child bounties. It must be greater
		///   than 0.
		/// - The curator deposit is re-evaluated for the new value and any additional deposit is
		///   collected from the curator.
		/// - The value can only be increased, never decreased, so the invariant that the sum of
		///   child-bounty values never exceeds the parent value is preserved.
		/// - This call does **not** check that the bounty account holds `new_value`; it only
		///   updates the recorded value. Payouts stay bounded by the account's real balance at
		///   settlement, so increasing the value beyond the available funds simply makes a later
		///   payout fail — no funds are moved by this call.
		/// - Only a parent bounty's value can be increased via this call.
		///
		/// ### Parameters
		/// - `parent_bounty_id`: Index of the bounty whose value is increased.
		/// - `amount`: The amount to add to the bounty value.
		///
		/// ## Events
		///
		/// Emits [`Event::BountyValueIncreased`] if successful.
		#[pallet::call_index(9)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::increase_value())]
		pub fn increase_value(
			origin: OriginFor<T>,
			#[pallet::compact] parent_bounty_id: BountyIndex,
			#[pallet::compact] amount: T::Balance,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;
			ensure!(!amount.is_zero(), Error::<T, I>::InvalidValue);

			let (old_value, new_value) = Bounties::<T, I>::try_mutate(
				parent_bounty_id,
				|maybe_bounty| -> Result<(T::Balance, T::Balance), DispatchError> {
					let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;

					// Only an `Active` bounty has a committed curator who can authorize and
					// collateralize the increase.
					let curator = match &bounty.status {
						BountyStatus::Active { curator } => curator.clone(),
						_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
					};
					ensure!(signer == curator, Error::<T, I>::RequireCurator);

					// Reject an overflowing increase rather than silently saturating to a
					// nonsensical value.
					let old_value = bounty.value;
					let new_value =
						old_value.checked_add(&amount).ok_or(Error::<T, I>::InvalidValue)?;

					// Re-evaluate the curator deposit for the new value, collecting any additional
					// hold from the curator. The deposit always exists for an `Active` bounty.
					let native_amount = T::BalanceConverter::from_asset_balance(
						new_value,
						bounty.asset_kind.clone(),
					)
					.map_err(|_| Error::<T, I>::FailedToConvertBalance)?;
					let deposit =
						CuratorDeposit::<T, I>::take(parent_bounty_id, None::<BountyIndex>)
							.ok_or(Error::<T, I>::UnexpectedStatus)?;
					let deposit = deposit.update(&curator, native_amount)?;
					CuratorDeposit::<T, I>::insert(parent_bounty_id, None::<BountyIndex>, deposit);

					bounty.value = new_value;
					Ok((old_value, new_value))
				},
			)?;

			Self::deposit_event(Event::<T, I>::BountyValueIncreased {
				index: parent_bounty_id,
				old_value,
				new_value,
			});

			Ok(())
		}
	}

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<SystemBlockNumberFor<T>> for Pallet<T, I> {
		#[cfg(feature = "try-runtime")]
		fn try_state(_n: SystemBlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}
}

#[cfg(any(feature = "try-runtime", test))]
impl<T: Config<I>, I: 'static> Pallet<T, I> {
	/// Ensure the correctness of the state of this pallet.
	///
	/// This should be valid before or after each state transition of this pallet.
	pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
		Self::try_state_bounties_count()?;

		for parent_bounty_id in Bounties::<T, I>::iter_keys() {
			Self::try_state_child_bounties_count(parent_bounty_id)?;
		}

		Ok(())
	}

	/// # Bounty Invariants
	///
	/// * `BountyCount` should be greater or equals to the length of the number of items in
	///   `Bounties`.
	fn try_state_bounties_count() -> Result<(), sp_runtime::TryRuntimeError> {
		let bounties_length = Bounties::<T, I>::iter().count() as u32;

		ensure!(
			<BountyCount<T, I>>::get() >= bounties_length,
			"`BountyCount` must be grater or equals the number of `Bounties` in storage"
		);

		Ok(())
	}

	/// # Child-Bounty Invariants for a given parent bounty
	///
	/// * `ChildBountyCount` should be greater or equals to the length of the number of items in
	///   `ChildBounties`.
	fn try_state_child_bounties_count(
		parent_bounty_id: BountyIndex,
	) -> Result<(), sp_runtime::TryRuntimeError> {
		let child_bounties_length =
			ChildBounties::<T, I>::iter_prefix(parent_bounty_id).count() as u32;

		ensure!(
			<ChildBountiesPerParent<T, I>>::get(parent_bounty_id) >= child_bounties_length,
			"`ChildBountiesPerParent` must be grater or equals the number of `ChildBounties` in storage"
		);

		Ok(())
	}
}

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	/// The account/location of the funding source.
	pub fn funding_source_account(
		asset_kind: T::AssetKind,
	) -> Result<T::Beneficiary, DispatchError> {
		T::FundingSource::try_convert(asset_kind)
			.map_err(|_| Error::<T, I>::FailedToConvertSource.into())
	}

	/// The account/location of a bounty.
	pub fn bounty_account(
		bounty_id: BountyIndex,
		asset_kind: T::AssetKind,
	) -> Result<T::Beneficiary, DispatchError> {
		T::BountySource::try_convert((bounty_id, asset_kind))
			.map_err(|_| Error::<T, I>::FailedToConvertSource.into())
	}

	/// The account/location of a child-bounty.
	pub fn child_bounty_account(
		parent_bounty_id: BountyIndex,
		child_bounty_id: BountyIndex,
		asset_kind: T::AssetKind,
	) -> Result<T::Beneficiary, DispatchError> {
		T::ChildBountySource::try_convert((parent_bounty_id, child_bounty_id, asset_kind))
			.map_err(|_| Error::<T, I>::FailedToConvertSource.into())
	}

	/// Returns the asset kind, value, status and parent curator (if parent bounty
	/// active) of a child-/bounty.
	///
	/// The asset kind derives from the parent bounty.
	pub fn get_bounty_details(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
	) -> Result<
		(
			T::AssetKind,
			T::Balance,
			T::Hash,
			BountyStatus<T::AccountId, PaymentIdOf<T, I>, T::Beneficiary>,
			Option<T::AccountId>,
		),
		DispatchError,
	> {
		let parent_bounty =
			Bounties::<T, I>::get(parent_bounty_id).ok_or(Error::<T, I>::InvalidIndex)?;

		// Ensures child-bounty uses parent curator only when parent bounty is active.
		let parent_curator = if let BountyStatus::Active { curator } = &parent_bounty.status {
			Some(curator.clone())
		} else {
			None
		};

		match child_bounty_id {
			None => Ok((
				parent_bounty.asset_kind,
				parent_bounty.value,
				parent_bounty.metadata,
				parent_bounty.status,
				parent_curator,
			)),
			Some(child_bounty_id) => {
				let child_bounty = ChildBounties::<T, I>::get(parent_bounty_id, child_bounty_id)
					.ok_or(Error::<T, I>::InvalidIndex)?;
				Ok((
					parent_bounty.asset_kind,
					child_bounty.value,
					child_bounty.metadata,
					child_bounty.status,
					parent_curator,
				))
			},
		}
	}

	/// Updates the status of a child-/bounty.
	pub fn update_bounty_status(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		new_status: BountyStatus<T::AccountId, PaymentIdOf<T, I>, T::Beneficiary>,
	) -> Result<(), DispatchError> {
		match child_bounty_id {
			None => {
				let mut bounty =
					Bounties::<T, I>::get(parent_bounty_id).ok_or(Error::<T, I>::InvalidIndex)?;
				bounty.status = new_status;
				Bounties::<T, I>::insert(parent_bounty_id, bounty);
			},
			Some(child_bounty_id) => {
				let mut bounty = ChildBounties::<T, I>::get(parent_bounty_id, child_bounty_id)
					.ok_or(Error::<T, I>::InvalidIndex)?;
				bounty.status = new_status;
				ChildBounties::<T, I>::insert(parent_bounty_id, child_bounty_id, bounty);
			},
		}

		Ok(())
	}

	/// Calculates amount the beneficiary receives during child-/bounty payout.
	fn calculate_payout(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		value: T::Balance,
	) -> T::Balance {
		match child_bounty_id {
			None => {
				// Get total child bounties value, and subtract it from the parent
				// value.
				let children_value = ChildBountiesValuePerParent::<T, I>::get(parent_bounty_id);
				debug_assert!(children_value <= value);
				let payout = value.saturating_sub(children_value);
				payout
			},
			Some(_) => value,
		}
	}

	/// Cleanup a child-/bounty from the storage.
	fn remove_bounty(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		metadata: T::Hash,
	) {
		match child_bounty_id {
			None => {
				Bounties::<T, I>::remove(parent_bounty_id);
				ChildBountiesPerParent::<T, I>::remove(parent_bounty_id);
				TotalChildBountiesPerParent::<T, I>::remove(parent_bounty_id);
				ChildBountiesValuePerParent::<T, I>::remove(parent_bounty_id);
			},
			Some(child_bounty_id) => {
				ChildBounties::<T, I>::remove(parent_bounty_id, child_bounty_id);
				ChildBountiesPerParent::<T, I>::mutate(parent_bounty_id, |count| {
					count.saturating_dec()
				});
			},
		}

		T::Preimages::unrequest(&metadata);
	}

	/// Initiates payment from the funding source to the child-/bounty account/location.
	fn do_process_funding_payment(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		asset_kind: T::AssetKind,
		value: T::Balance,
		maybe_payment_status: Option<PaymentState<PaymentIdOf<T, I>>>,
	) -> Result<PaymentState<PaymentIdOf<T, I>>, DispatchError> {
		if let Some(payment_status) = maybe_payment_status {
			ensure!(payment_status.is_pending_or_failed(), Error::<T, I>::UnexpectedStatus);
		}

		let (source, beneficiary) = match child_bounty_id {
			None => (
				Self::funding_source_account(asset_kind.clone())?,
				Self::bounty_account(parent_bounty_id, asset_kind.clone())?,
			),
			Some(child_bounty_id) => (
				Self::bounty_account(parent_bounty_id, asset_kind.clone())?,
				Self::child_bounty_account(parent_bounty_id, child_bounty_id, asset_kind.clone())?,
			),
		};

		let id = <T as Config<I>>::Paymaster::pay(&source, &beneficiary, asset_kind, value)
			.map_err(|_| Error::<T, I>::FundingError)?;

		Self::deposit_event(Event::<T, I>::Paid {
			index: parent_bounty_id,
			child_index: child_bounty_id,
			payment_id: id,
		});

		Ok(PaymentState::Attempted { id })
	}

	/// Queries the status of the payment from the funding source to the child-/bounty
	/// account/location and returns a new payment status.
	fn do_check_funding_payment_status(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		payment_status: PaymentState<PaymentIdOf<T, I>>,
	) -> Result<PaymentState<PaymentIdOf<T, I>>, DispatchError> {
		let payment_id = payment_status.get_attempt_id().ok_or(Error::<T, I>::UnexpectedStatus)?;

		match <T as Config<I>>::Paymaster::check_payment(payment_id) {
			PaymentStatus::Success => {
				Self::deposit_event(Event::<T, I>::BountyFundingProcessed {
					index: parent_bounty_id,
					child_index: child_bounty_id,
				});
				Ok(PaymentState::Succeeded)
			},
			PaymentStatus::InProgress | PaymentStatus::Unknown => {
				return Err(Error::<T, I>::FundingInconclusive.into())
			},
			PaymentStatus::Failure => {
				Self::deposit_event(Event::<T, I>::PaymentFailed {
					index: parent_bounty_id,
					child_index: child_bounty_id,
					payment_id,
				});
				return Ok(PaymentState::Failed);
			},
		}
	}

	/// Initializes payment from the child-/bounty account/location to the funding source (i.e.
	/// treasury pot, parent bounty).
	fn do_process_refund_payment(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		asset_kind: T::AssetKind,
		value: T::Balance,
		payment_status: Option<PaymentState<PaymentIdOf<T, I>>>,
	) -> Result<PaymentState<PaymentIdOf<T, I>>, DispatchError> {
		if let Some(payment_status) = payment_status {
			ensure!(payment_status.is_pending_or_failed(), Error::<T, I>::UnexpectedStatus);
		}

		let (source, beneficiary) = match child_bounty_id {
			None => (
				Self::bounty_account(parent_bounty_id, asset_kind.clone())?,
				Self::funding_source_account(asset_kind.clone())?,
			),
			Some(child_bounty_id) => (
				Self::child_bounty_account(parent_bounty_id, child_bounty_id, asset_kind.clone())?,
				Self::bounty_account(parent_bounty_id, asset_kind.clone())?,
			),
		};

		let id = <T as Config<I>>::Paymaster::pay(&source, &beneficiary, asset_kind, value)
			.map_err(|_| Error::<T, I>::RefundError)?;

		Self::deposit_event(Event::<T, I>::Paid {
			index: parent_bounty_id,
			child_index: child_bounty_id,
			payment_id: id,
		});

		Ok(PaymentState::Attempted { id })
	}

	/// Queries the status of the refund payment from the child-/bounty account/location to the
	/// funding source and returns a new payment status.
	fn do_check_refund_payment_status(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		payment_status: PaymentState<PaymentIdOf<T, I>>,
	) -> Result<PaymentState<PaymentIdOf<T, I>>, DispatchError> {
		let payment_id = payment_status.get_attempt_id().ok_or(Error::<T, I>::UnexpectedStatus)?;

		match <T as pallet::Config<I>>::Paymaster::check_payment(payment_id) {
			PaymentStatus::Success => {
				Self::deposit_event(Event::<T, I>::BountyRefundProcessed {
					index: parent_bounty_id,
					child_index: child_bounty_id,
				});
				Ok(PaymentState::Succeeded)
			},
			PaymentStatus::InProgress | PaymentStatus::Unknown =>
			// nothing new to report
			{
				Err(Error::<T, I>::RefundInconclusive.into())
			},
			PaymentStatus::Failure => {
				// assume payment has failed, allow user to retry
				Self::deposit_event(Event::<T, I>::PaymentFailed {
					index: parent_bounty_id,
					child_index: child_bounty_id,
					payment_id,
				});
				Ok(PaymentState::Failed)
			},
		}
	}

	/// Initializes payment from the child-/bounty to the beneficiary account/location.
	fn do_process_payout_payment(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		asset_kind: T::AssetKind,
		value: T::Balance,
		beneficiary: T::Beneficiary,
		payment_status: Option<PaymentState<PaymentIdOf<T, I>>>,
	) -> Result<PaymentState<PaymentIdOf<T, I>>, DispatchError> {
		if let Some(payment_status) = payment_status {
			ensure!(payment_status.is_pending_or_failed(), Error::<T, I>::UnexpectedStatus);
		}

		let payout = Self::calculate_payout(parent_bounty_id, child_bounty_id, value);

		let source = match child_bounty_id {
			None => Self::bounty_account(parent_bounty_id, asset_kind.clone())?,
			Some(child_bounty_id) => {
				Self::child_bounty_account(parent_bounty_id, child_bounty_id, asset_kind.clone())?
			},
		};

		let id = <T as Config<I>>::Paymaster::pay(&source, &beneficiary, asset_kind, payout)
			.map_err(|_| Error::<T, I>::PayoutError)?;

		Self::deposit_event(Event::<T, I>::Paid {
			index: parent_bounty_id,
			child_index: child_bounty_id,
			payment_id: id,
		});

		Ok(PaymentState::Attempted { id })
	}

	/// Queries the status of the payment from the child-/bounty to the beneficiary account/location
	/// and returns a new payment status.
	fn do_check_payout_payment_status(
		parent_bounty_id: BountyIndex,
		child_bounty_id: Option<BountyIndex>,
		asset_kind: T::AssetKind,
		value: T::Balance,
		beneficiary: T::Beneficiary,
		payment_status: PaymentState<PaymentIdOf<T, I>>,
	) -> Result<PaymentState<PaymentIdOf<T, I>>, DispatchError> {
		let payment_id = payment_status.get_attempt_id().ok_or(Error::<T, I>::UnexpectedStatus)?;

		match <T as pallet::Config<I>>::Paymaster::check_payment(payment_id) {
			PaymentStatus::Success => {
				let payout = Self::calculate_payout(parent_bounty_id, child_bounty_id, value);

				Self::deposit_event(Event::<T, I>::BountyPayoutProcessed {
					index: parent_bounty_id,
					child_index: child_bounty_id,
					asset_kind: asset_kind.clone(),
					value: payout,
					beneficiary,
				});

				Ok(PaymentState::Succeeded)
			},
			PaymentStatus::InProgress | PaymentStatus::Unknown =>
			// nothing new to report
			{
				Err(Error::<T, I>::PayoutInconclusive.into())
			},
			PaymentStatus::Failure => {
				// assume payment has failed, allow user to retry
				Self::deposit_event(Event::<T, I>::PaymentFailed {
					index: parent_bounty_id,
					child_index: child_bounty_id,
					payment_id,
				});
				Ok(PaymentState::Failed)
			},
		}
	}
}

/// Type implementing curator deposit as a percentage of the child-/bounty value.
///
/// It implements `Convert` trait and can be used with types like `HoldConsideration` implementing
/// `Consideration` trait.
pub struct CuratorDepositAmount<Mult, Min, Max, Balance>(PhantomData<(Mult, Min, Max, Balance)>);
impl<Mult, Min, Max, Balance> Convert<Balance, Balance>
	for CuratorDepositAmount<Mult, Min, Max, Balance>
where
	Balance: frame_support::traits::tokens::Balance,
	Min: Get<Option<Balance>>,
	Max: Get<Option<Balance>>,
	Mult: Get<Permill>,
{
	fn convert(value: Balance) -> Balance {
		let mut deposit = Mult::get().mul_floor(value);

		if let Some(min) = Min::get() {
			if deposit < min {
				deposit = min;
			}
		}

		if let Some(max) = Max::get() {
			if deposit > max {
				deposit = max;
			}
		}

		deposit
	}
}

/// Derives the funding `AccountId` from the `PalletId` and converts it into the
/// bounty `Beneficiary`, used as the source of bounty funds.
///
/// Used when the [`PalletId`] itself owns the funds (i.e. pallet-treasury id).
/// # Type Parameters
/// - `Id`: The pallet ID getter
/// - `T`: The pallet configuration
/// - `C`: Converter from `T::AccountId` to `T::Beneficiary`. Use `Identity` when types are the
///   same.
/// - `I`: Instance parameter (default: `()`)
pub struct PalletIdAsFundingSource<Id, T, C, I = ()>(PhantomData<(Id, T, C, I)>);
impl<Id, T, C, I> TryConvert<T::AssetKind, T::Beneficiary> for PalletIdAsFundingSource<Id, T, C, I>
where
	Id: Get<PalletId>,
	T: crate::Config<I>,
	C: Convert<T::AccountId, T::Beneficiary>,
{
	fn try_convert(_asset_kind: T::AssetKind) -> Result<T::Beneficiary, T::AssetKind> {
		let account: T::AccountId = Id::get().into_account_truncating();
		Ok(C::convert(account))
	}
}

/// Standard 3-byte prefix for bounty account derivation.
///
/// Returns `b"mbt"` (multi-asset bounty). Use this type when configuring
/// [`BountySourceFromPalletId`] unless your runtime requires a custom prefix.
pub struct BountyAccountPrefix;
impl Get<[u8; 3]> for BountyAccountPrefix {
	fn get() -> [u8; 3] {
		*b"mbt"
	}
}

/// Standard 3-byte prefix for child-bounty account derivation.
///
/// Returns `b"mcb"` (multi-asset child bounty). Use this type when configuring
/// [`ChildBountySourceFromPalletId`] unless your runtime requires a custom prefix.
pub struct ChildBountyAccountPrefix;
impl Get<[u8; 3]> for ChildBountyAccountPrefix {
	fn get() -> [u8; 3] {
		*b"mcb"
	}
}

/// Derives a bounty `AccountId` from the `PalletId` and the `BountyIndex`,
/// then converts it into the corresponding bounty `Beneficiary`.
///
/// The account is derived using a fixed-size 3-byte prefix (e.g. `b"mbt"` for multi-asset bounty).
/// The prefix is supplied via the `Prefix` type parameter, which must implement `Get<[u8; 3]>`.
/// This ensures the encoded sub-account seed has a predictable size and avoids truncation issues.
///
/// Used when the [`PalletId`] itself owns the funds (i.e. pallet-treasury id).
///
/// # Type Parameters
/// - `Id`: The pallet ID getter
/// - `Prefix`: Getter for the 3-byte account prefix (e.g. [`BountyAccountPrefix`]). Must implement
///   `Get<[u8; 3]>`. Fixed at 3 bytes to guarantee predictable seed size and avoid truncation of
///   the bounty index.
/// - `T`: The pallet configuration
/// - `C`: Converter from `T::AccountId` to `T::Beneficiary`. Use `Identity` when types are the
///   same.
/// - `I`: Instance parameter (default: `()`)
pub struct BountySourceFromPalletId<Id, Prefix, T, C, I = ()>(PhantomData<(Id, Prefix, T, C, I)>);
impl<Id, Prefix, T, C, I> TryConvert<(BountyIndex, T::AssetKind), T::Beneficiary>
	for BountySourceFromPalletId<Id, Prefix, T, C, I>
where
	Id: Get<PalletId>,
	Prefix: Get<[u8; 3]>,
	T: crate::Config<I>,
	C: Convert<T::AccountId, T::Beneficiary>,
{
	fn try_convert(
		(parent_bounty_id, _asset_kind): (BountyIndex, T::AssetKind),
	) -> Result<T::Beneficiary, (BountyIndex, T::AssetKind)> {
		let account: T::AccountId =
			Id::get().into_sub_account_truncating((Prefix::get(), parent_bounty_id));
		Ok(C::convert(account))
	}
}

/// Derives a child-bounty `AccountId` from the `PalletId`, the parent index,
/// and the child index, then converts it into the child-bounty `Beneficiary`.
///
/// The account is derived using a fixed-size 3-byte prefix (e.g. `b"mcb"` for multi-asset child
/// bounty). The prefix is supplied via the `Prefix` type parameter, which must implement
/// `Get<[u8; 3]>`. Using a different prefix from the parent bounty ensures distinct account IDs
/// when parent and child indices coincide.
///
/// Used when the [`PalletId`] itself owns the funds (i.e. pallet-treasury id).
///
/// # Type Parameters
/// - `Id`: The pallet ID getter
/// - `Prefix`: Getter for the 3-byte account prefix (e.g. [`ChildBountyAccountPrefix`]). Must
///   implement `Get<[u8; 3]>`. Fixed at 3 bytes to guarantee predictable seed size and avoid
///   truncation of the bounty indices.
/// - `T`: The pallet configuration
/// - `C`: Converter from `T::AccountId` to `T::Beneficiary`. Use `Identity` when types are the
///   same.
/// - `I`: Instance parameter (default: `()`)
pub struct ChildBountySourceFromPalletId<Id, Prefix, T, C, I = ()>(
	PhantomData<(Id, Prefix, T, C, I)>,
);
impl<Id, Prefix, T, C, I> TryConvert<(BountyIndex, BountyIndex, T::AssetKind), T::Beneficiary>
	for ChildBountySourceFromPalletId<Id, Prefix, T, C, I>
where
	Id: Get<PalletId>,
	Prefix: Get<[u8; 3]>,
	T: crate::Config<I>,
	C: Convert<T::AccountId, T::Beneficiary>,
{
	fn try_convert(
		(parent_bounty_id, child_bounty_id, _asset_kind): (BountyIndex, BountyIndex, T::AssetKind),
	) -> Result<T::Beneficiary, (BountyIndex, BountyIndex, T::AssetKind)> {
		// The prefix is distinct from the bounty prefix so AccountIds differ when parent and
		// child index are the same.
		let account: T::AccountId = Id::get().into_sub_account_truncating((
			Prefix::get(),
			parent_bounty_id,
			child_bounty_id,
		));
		Ok(C::convert(account))
	}
}
