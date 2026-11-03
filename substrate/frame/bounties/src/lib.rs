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

//! # Bounties Module ( pallet-bounties )
//!
//! ## Bounty
//!
//! > NOTE: This pallet is tightly coupled with pallet-treasury.
//!
//! A Bounty Spending is a reward for a specified body of work - or specified set of objectives -
//! that needs to be executed for a predefined Treasury amount to be paid out. A curator is assigned
//! after the bounty is approved and funded by Council, to be delegated with the responsibility of
//! assigning a payout address once the specified set of objectives is completed.
//!
//! After the Council has activated a bounty, it delegates the work that requires expertise to a
//! curator in exchange of a deposit. Once the curator accepts the bounty, they get to close the
//! active bounty. Closing the active bounty enacts a delayed payout to the payout address, the
//! curator fee and the return of the curator deposit. The delay allows for intervention through
//! regular democracy. The Council gets to unassign the curator, resulting in a new curator
//! election. The Council also gets to cancel the bounty if deemed necessary before assigning a
//! curator or once the bounty is active or payout is pending, resulting in the slash of the
//! curator's deposit.
//!
//! This pallet may opt into using a [`ChildBountyManager`] that enables bounties to be split into
//! sub-bounties, as children of an established bounty (called the parent in the context of it's
//! children).
//!
//! > NOTE: The parent bounty cannot be closed if it has a non-zero number of it has active child
//! > bounties associated with it.
//!
//! ### Terminology
//!
//! Bounty:
//!
//! - **Bounty spending proposal:** A proposal to reward a predefined body of work upon completion
//!   by the Treasury.
//! - **Proposer:** An account proposing a bounty spending.
//! - **Curator:** An account managing the bounty and assigning a payout address receiving the
//!   reward for the completion of work.
//! - **Deposit:** The amount held on deposit for placing a bounty proposal plus the amount held on
//!   deposit per byte within the bounty description.
//! - **Curator deposit:** The payment from a candidate willing to curate an approved bounty. The
//!   deposit is returned when/if the bounty is completed.
//! - **Bounty value:** The total amount that should be paid to the Payout Address if the bounty is
//!   rewarded.
//! - **Payout address:** The account to which the total or part of the bounty is assigned to.
//! - **Payout Delay:** The delay period for which a bounty beneficiary needs to wait before
//!   claiming.
//! - **Curator fee:** The reserved upfront payment for a curator for work related to the bounty.
//!
//! ## Interface
//!
//! ### Dispatchable Functions
//!
//! Bounty protocol:
//!
//! - `propose_bounty` - Propose a specific treasury amount to be earmarked for a predefined set of
//!   tasks and stake the required deposit.
//! - `approve_bounty` - Accept a specific treasury amount to be earmarked for a predefined body of
//!   work.
//! - `propose_curator` - Assign an account to a bounty as candidate curator.
//! - `approve_bounty_with_curator` - Accept a specific treasury amount for a predefined body of
//!   work with assigned candidate curator account.
//! - `accept_curator` - Accept a bounty assignment from the Council, setting a curator deposit.
//! - `extend_bounty_expiry` - Extend the expiry block number of the bounty and stay active.
//! - `award_bounty` - Close and pay out the specified amount for the completed work.
//! - `claim_bounty` - Claim a specific bounty amount from the Payout Address.
//! - `unassign_curator` - Unassign an accepted curator from a specific earmark.
//! - `close_bounty` - Cancel the earmark for a specific treasury amount and close the bounty.

#![recursion_limit = "512"]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod migrations;
mod tests;
pub mod weights;

extern crate alloc;

use alloc::vec::Vec;

use frame_support::traits::{
	fungible::Mutate as FungibleMutate,
	fungibles::{
		Create as FungiblesCreate, Inspect as FungiblesInspect, Mutate as FungiblesMutate,
	},
	tokens::{Fortitude, Preservation},
	Currency,
	ExistenceRequirement::AllowDeath,
	Get, Imbalance, OnUnbalanced, ReservableCurrency,
};

use sp_runtime::{
	traits::{AccountIdConversion, BadOrigin, BlockNumberProvider, Saturating, StaticLookup, Zero},
	Debug, DispatchResult, Permill,
};

use frame_support::{
	dispatch::DispatchResultWithPostInfo, pallet_prelude::*, traits::EnsureOrigin,
};
use frame_system::pallet_prelude::{
	ensure_signed, BlockNumberFor as SystemBlockNumberFor, OriginFor,
};
use scale_info::TypeInfo;
pub use weights::WeightInfo;

pub use pallet::*;

type BalanceOf<T, I = ()> = pallet_treasury::BalanceOf<T, I>;

type PositiveImbalanceOf<T, I = ()> = pallet_treasury::PositiveImbalanceOf<T, I>;

/// An index of a bounty. Just a `u32`.
pub type BountyIndex = u32;

type AccountIdLookupOf<T> = <<T as frame_system::Config>::Lookup as StaticLookup>::Source;

type BlockNumberFor<T, I = ()> =
	<<T as pallet_treasury::Config<I>>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

/// A bounty proposal.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct Bounty<AccountId, Balance, BlockNumber> {
	/// The account proposing it.
	pub proposer: AccountId,
	/// The (total) amount that should be paid if the bounty is rewarded.
	pub value: Balance,
	/// The curator fee. Included in value.
	pub fee: Balance,
	/// The deposit of curator.
	pub curator_deposit: Balance,
	/// The amount held on deposit (reserved) for making this proposal.
	bond: Balance,
	/// The status of this bounty.
	status: BountyStatus<AccountId, BlockNumber>,
}

impl<AccountId: PartialEq + Clone + Ord, Balance, BlockNumber: Clone>
	Bounty<AccountId, Balance, BlockNumber>
{
	/// Getter for bounty status, to be used for child bounties.
	pub fn get_status(&self) -> BountyStatus<AccountId, BlockNumber> {
		self.status.clone()
	}
}

/// The status of a bounty proposal.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub enum BountyStatus<AccountId, BlockNumber> {
	/// The bounty is proposed and waiting for approval.
	Proposed,
	/// The bounty is approved and waiting to become active at next spend period.
	Approved,
	/// The bounty is funded and waiting for curator assignment.
	Funded,
	/// A curator has been proposed. Waiting for acceptance from the curator.
	CuratorProposed {
		/// The assigned curator of this bounty.
		curator: AccountId,
	},
	/// The bounty is active and waiting to be awarded.
	Active {
		/// The curator of this bounty.
		curator: AccountId,
		/// An update from the curator is due by this block, else they are considered inactive.
		update_due: BlockNumber,
	},
	/// The bounty is awarded and waiting to released after a delay.
	PendingPayout {
		/// The curator of this bounty.
		curator: AccountId,
		/// The beneficiary of the bounty.
		beneficiary: AccountId,
		/// When the bounty can be claimed.
		unlock_at: BlockNumber,
	},
	/// The bounty is approved with curator assigned.
	ApprovedWithCurator {
		/// The assigned curator of this bounty.
		curator: AccountId,
	},
}

/// The child bounty manager.
pub trait ChildBountyManager<Balance> {
	/// Get the active child bounties for a parent bounty.
	fn child_bounties_count(bounty_id: BountyIndex) -> BountyIndex;

	/// Take total curator fees of children-bounty curators.
	fn children_curator_fees(bounty_id: BountyIndex) -> Balance;

	/// Hook called when a parent bounty is removed.
	fn bounty_removed(bounty_id: BountyIndex);
}

/// Transfer all assets held by an account (e.g. a stale bounty sub-account) back to another.
pub trait TransferAllAssets<AccountId> {
	/// Transfer all assets from one account to another, possibly reaping `from`.
	///
	/// Returns `true` if any balance was actually transferred, `false` if the account was already
	/// empty.
	fn force_transfer_all_assets(from: &AccountId, to: &AccountId) -> Result<bool, DispatchError>;

	/// Mint benchmark amounts of each relevant asset into `from`.
	///
	/// Called during benchmarking so that `force_transfer_all_assets` exercises real transfers
	/// instead of being a no-op. The default implementation is a no-op.
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(_from: &AccountId) {}
}

impl<AccountId> TransferAllAssets<AccountId> for () {
	fn force_transfer_all_assets(_: &AccountId, _: &AccountId) -> Result<bool, DispatchError> {
		Ok(false)
	}
}

/// Transfer the entire balance of a single `fungible::Mutate` currency from one account to
/// another.
///
/// Suitable for runtimes that expose exactly one relevant currency (e.g. native-only runtimes
/// without multi-asset support). For runtimes with multi-asset support, prefer
/// [`TransferAllFungibles`] with all relevant asset IDs in `RelevantAssets`.
pub struct TransferFungible<AccountId, Currency>(core::marker::PhantomData<(AccountId, Currency)>);
impl<AccountId, C> TransferAllAssets<AccountId> for TransferFungible<AccountId, C>
where
	C: FungibleMutate<AccountId>,
	AccountId: Eq,
{
	fn force_transfer_all_assets(from: &AccountId, to: &AccountId) -> Result<bool, DispatchError> {
		let balance = C::reducible_balance(from, Preservation::Expendable, Fortitude::Polite);
		if balance.is_zero() {
			return Ok(false);
		}
		C::transfer(from, to, balance, Preservation::Expendable)?;
		Ok(true)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(from: &AccountId) {
		let _ = C::mint_into(from, 1_000_000u32.into());
	}
}

/// Transfer all `RelevantAssets` of the `Fungibles` from one account to another.
///
/// The native asset should be the first in the list of `RelevantAssets`, otherwise the transfers
/// of the other maybe fails.
pub struct TransferAllFungibles<AccountId, Fungibles, RelevantAssets>(
	core::marker::PhantomData<(AccountId, Fungibles, RelevantAssets)>,
);
impl<AccountId, Fungibles, RelevantAssets> TransferAllAssets<AccountId>
	for TransferAllFungibles<AccountId, Fungibles, RelevantAssets>
where
	Fungibles: FungiblesMutate<AccountId> + FungiblesCreate<AccountId>,
	RelevantAssets: Get<Vec<<Fungibles as FungiblesInspect<AccountId>>::AssetId>>,
	AccountId: Eq + Clone,
{
	fn force_transfer_all_assets(from: &AccountId, to: &AccountId) -> Result<bool, DispatchError> {
		// We iterate through all assets twice in case that the Native asset was not last in the
		// list and ED remained because of an insufficient asset at the end of the list.
		let assets_twice =
			RelevantAssets::get().into_iter().chain(RelevantAssets::get().into_iter());

		let mut transferred_any = false;
		for id in assets_twice {
			let balance = Fungibles::reducible_balance(
				id.clone(),
				from,
				Preservation::Expendable,
				Fortitude::Polite,
			);
			if balance.is_zero() {
				continue;
			}

			// Ignore errors since this can only fail if the receiver does not exist.
			if Fungibles::transfer(id, from, to, balance, Preservation::Expendable).is_ok() {
				transferred_any = true;
			}
		}
		Ok(transferred_any)
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(from: &AccountId) {
		for id in RelevantAssets::get() {
			if !Fungibles::asset_exists(id.clone()) {
				// For native assets, Create is a no-op; for fungible assets this ensures the
				// asset exists before minting so the benchmark exercises real transfers.
				Fungibles::create(id.clone(), from.clone(), true, 1u32.into())
					.expect("asset creation should succeed in benchmarks");
			}
			let _ = Fungibles::mint_into(id, from, 1_000u32.into());
		}
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	const STORAGE_VERSION: StorageVersion = StorageVersion::new(4);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T, I = ()>(_);

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config + pallet_treasury::Config<I> {
		/// The amount held on deposit for placing a bounty proposal.
		#[pallet::constant]
		type BountyDepositBase: Get<BalanceOf<Self, I>>;

		/// The delay period for which a bounty beneficiary need to wait before claim the payout.
		#[pallet::constant]
		type BountyDepositPayoutDelay: Get<BlockNumberFor<Self, I>>;

		/// The time limit for a curator to act before a bounty expires.
		///
		/// The period that starts when a curator is approved, during which they must execute or
		/// update the bounty via `extend_bounty_expiry`. If missed, the bounty expires, and the
		/// curator may be slashed. If `BlockNumberFor::MAX`, bounties stay active indefinitely,
		/// removing the need for `extend_bounty_expiry`.
		#[pallet::constant]
		type BountyUpdatePeriod: Get<BlockNumberFor<Self, I>>;

		/// The curator deposit is calculated as a percentage of the curator fee.
		///
		/// This deposit has optional upper and lower bounds with `CuratorDepositMax` and
		/// `CuratorDepositMin`.
		#[pallet::constant]
		type CuratorDepositMultiplier: Get<Permill>;

		/// Maximum amount of funds that should be placed in a deposit for making a proposal.
		#[pallet::constant]
		type CuratorDepositMax: Get<Option<BalanceOf<Self, I>>>;

		/// Minimum amount of funds that should be placed in a deposit for making a proposal.
		#[pallet::constant]
		type CuratorDepositMin: Get<Option<BalanceOf<Self, I>>>;

		/// Minimum value for a bounty.
		#[pallet::constant]
		type BountyValueMinimum: Get<BalanceOf<Self, I>>;

		/// The amount held on deposit per byte within the tip report reason or bounty description.
		#[pallet::constant]
		type DataDepositPerByte: Get<BalanceOf<Self, I>>;

		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Maximum acceptable reason length.
		///
		/// Benchmarks depend on this value, be sure to update weights file when changing this value
		#[pallet::constant]
		type MaximumReasonLength: Get<u32>;

		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// The child bounty manager.
		type ChildBountyManager: ChildBountyManager<BalanceOf<Self, I>>;

		/// Handler for the unbalanced decrease when slashing for a rejected bounty.
		type OnSlash: OnUnbalanced<pallet_treasury::NegativeImbalanceOf<Self, I>>;

		/// Means to transfer all assets from one account to another.
		///
		/// This is only used for bounty closure to ensure that all assets are returned to the
		/// treasury.
		type TransferAllAssets: TransferAllAssets<Self::AccountId>;
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Proposer's balance is too low.
		InsufficientProposersBalance,
		/// No proposal or bounty at that index.
		InvalidIndex,
		/// The reason given is just too big.
		ReasonTooBig,
		/// The bounty status is unexpected.
		UnexpectedStatus,
		/// Require bounty curator.
		RequireCurator,
		/// Invalid bounty value.
		InvalidValue,
		/// Invalid bounty fee.
		InvalidFee,
		/// A bounty payout is pending.
		/// To cancel the bounty, you must unassign and slash the curator.
		PendingPayout,
		/// The bounties cannot be claimed/closed because it's still in the countdown period.
		Premature,
		/// The bounty cannot be closed because it has active child bounties.
		HasActiveChildBounty,
		/// Too many approvals are already queued.
		TooManyQueued,
		/// User is not the proposer of the bounty.
		NotProposer,
		/// The bounty is still active and its account cannot be reclaimed.
		BountyStillActive,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// New bounty proposal.
		BountyProposed { index: BountyIndex },
		/// A bounty proposal was rejected; funds were slashed.
		BountyRejected { index: BountyIndex, bond: BalanceOf<T, I> },
		/// A bounty proposal is funded and became active.
		BountyBecameActive { index: BountyIndex },
		/// A bounty is awarded to a beneficiary.
		BountyAwarded { index: BountyIndex, beneficiary: T::AccountId },
		/// A bounty is claimed by beneficiary.
		BountyClaimed { index: BountyIndex, payout: BalanceOf<T, I>, beneficiary: T::AccountId },
		/// A bounty is cancelled.
		BountyCanceled { index: BountyIndex },
		/// A bounty expiry is extended.
		BountyExtended { index: BountyIndex },
		/// A bounty is approved.
		BountyApproved { index: BountyIndex },
		/// A bounty curator is proposed.
		CuratorProposed { bounty_id: BountyIndex, curator: T::AccountId },
		/// A bounty curator is unassigned.
		CuratorUnassigned { bounty_id: BountyIndex },
		/// A bounty curator is accepted.
		CuratorAccepted { bounty_id: BountyIndex, curator: T::AccountId },
		/// A bounty deposit has been poked.
		DepositPoked {
			bounty_id: BountyIndex,
			proposer: T::AccountId,
			old_deposit: BalanceOf<T, I>,
			new_deposit: BalanceOf<T, I>,
		},
		/// Stranded funds left in a closed bounty's account were reclaimed to the treasury.
		BountyFundsReclaimed { bounty_id: BountyIndex },
	}

	/// Number of bounty proposals that have been made.
	#[pallet::storage]
	pub type BountyCount<T: Config<I>, I: 'static = ()> = StorageValue<_, BountyIndex, ValueQuery>;

	/// Bounties that have been made.
	#[pallet::storage]
	pub type Bounties<T: Config<I>, I: 'static = ()> = StorageMap<
		_,
		Twox64Concat,
		BountyIndex,
		Bounty<T::AccountId, BalanceOf<T, I>, BlockNumberFor<T, I>>,
	>;

	/// The description of each bounty.
	#[pallet::storage]
	pub type BountyDescriptions<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, BountyIndex, BoundedVec<u8, T::MaximumReasonLength>>;

	/// Bounty indices that have been approved but not yet funded.
	#[pallet::storage]
	#[allow(deprecated)]
	pub type BountyApprovals<T: Config<I>, I: 'static = ()> =
		StorageValue<_, BoundedVec<BountyIndex, T::MaxApprovals>, ValueQuery>;

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Propose a new bounty.
		///
		/// The dispatch origin for this call must be _Signed_.
		///
		/// Payment: `TipReportDepositBase` will be reserved from the origin account, as well as
		/// `DataDepositPerByte` for each byte in `reason`. It will be unreserved upon approval,
		/// or slashed when rejected.
		///
		/// - `curator`: The curator account whom will manage this bounty.
		/// - `fee`: The curator fee.
		/// - `value`: The total payment amount of this bounty, curator fee included.
		/// - `description`: The description of this bounty.
		#[pallet::call_index(0)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::propose_bounty(description.len() as u32))]
		pub fn propose_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] value: BalanceOf<T, I>,
			description: Vec<u8>,
		) -> DispatchResult {
			let proposer = ensure_signed(origin)?;
			Self::create_bounty(proposer, description, value)?;
			Ok(())
		}

		/// Approve a bounty proposal. At a later time, the bounty will be funded and become active
		/// and the original deposit will be returned.
		///
		/// May only be called from `T::SpendOrigin`.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::approve_bounty())]
		pub fn approve_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResult {
			let max_amount = T::SpendOrigin::ensure_origin(origin)?;
			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;
				ensure!(
					bounty.value <= max_amount,
					pallet_treasury::Error::<T, I>::InsufficientPermission
				);
				ensure!(bounty.status == BountyStatus::Proposed, Error::<T, I>::UnexpectedStatus);

				bounty.status = BountyStatus::Approved;

				BountyApprovals::<T, I>::try_append(bounty_id)
					.map_err(|()| Error::<T, I>::TooManyQueued)?;

				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::BountyApproved { index: bounty_id });
			Ok(())
		}

		/// Propose a curator to a funded bounty.
		///
		/// May only be called from `T::SpendOrigin`.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(2)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::propose_curator())]
		pub fn propose_curator(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
			curator: AccountIdLookupOf<T>,
			#[pallet::compact] fee: BalanceOf<T, I>,
		) -> DispatchResult {
			let max_amount = T::SpendOrigin::ensure_origin(origin)?;

			let curator = T::Lookup::lookup(curator)?;
			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;
				ensure!(
					bounty.value <= max_amount,
					pallet_treasury::Error::<T, I>::InsufficientPermission
				);
				match bounty.status {
					BountyStatus::Funded => {},
					_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
				};

				ensure!(fee < bounty.value, Error::<T, I>::InvalidFee);

				bounty.status = BountyStatus::CuratorProposed { curator: curator.clone() };
				bounty.fee = fee;

				Self::deposit_event(Event::<T, I>::CuratorProposed { bounty_id, curator });

				Ok(())
			})?;
			Ok(())
		}

		/// Unassign curator from a bounty.
		///
		/// This function can only be called by the `RejectOrigin` a signed origin.
		///
		/// If this function is called by the `RejectOrigin`, we assume that the curator is
		/// malicious or inactive. As a result, we will slash the curator when possible.
		///
		/// If the origin is the curator, we take this as a sign they are unable to do their job and
		/// they willingly give up. We could slash them, but for now we allow them to recover their
		/// deposit and exit without issue. (We may want to change this if it is abused.)
		///
		/// Finally, the origin can be anyone if and only if the curator is "inactive". This allows
		/// anyone in the community to call out that a curator is not doing their due diligence, and
		/// we should pick a new curator. In this case the curator should also be slashed.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(3)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::unassign_curator())]
		pub fn unassign_curator(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResult {
			let maybe_sender = ensure_signed(origin.clone())
				.map(Some)
				.or_else(|_| T::RejectOrigin::ensure_origin(origin).map(|_| None))?;

			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;

				let slash_curator =
					|curator: &T::AccountId, curator_deposit: &mut BalanceOf<T, I>| {
						let imbalance = T::Currency::slash_reserved(curator, *curator_deposit).0;
						T::OnSlash::on_unbalanced(imbalance);
						*curator_deposit = Zero::zero();
					};

				match bounty.status {
					BountyStatus::Proposed | BountyStatus::Approved | BountyStatus::Funded => {
						// No curator to unassign at this point.
						return Err(Error::<T, I>::UnexpectedStatus.into());
					},
					BountyStatus::ApprovedWithCurator { ref curator } => {
						// Bounty not yet funded, but bounty was approved with curator.
						// `RejectOrigin` or curator himself can unassign from this bounty.
						ensure!(maybe_sender.map_or(true, |sender| sender == *curator), BadOrigin);
						// This state can only be while the bounty is not yet funded so we return
						// bounty to the `Approved` state without curator
						bounty.status = BountyStatus::Approved;
						return Ok(());
					},
					BountyStatus::CuratorProposed { ref curator } => {
						// A curator has been proposed, but not accepted yet.
						// Either `RejectOrigin` or the proposed curator can unassign the curator.
						ensure!(maybe_sender.map_or(true, |sender| sender == *curator), BadOrigin);
					},
					BountyStatus::Active { ref curator, ref update_due } => {
						// The bounty is active.
						match maybe_sender {
							// If the `RejectOrigin` is calling this function, slash the curator.
							None => {
								slash_curator(curator, &mut bounty.curator_deposit);
								// Continue to change bounty status below...
							},
							Some(sender) => {
								// If the sender is not the curator, and the curator is inactive,
								// slash the curator.
								if sender != *curator {
									let block_number = Self::treasury_block_number();
									if *update_due < block_number {
										slash_curator(curator, &mut bounty.curator_deposit);
									// Continue to change bounty status below...
									} else {
										// Curator has more time to give an update.
										return Err(Error::<T, I>::Premature.into());
									}
								} else {
									// Else this is the curator, willingly giving up their role.
									// Give back their deposit.
									let err_amount =
										T::Currency::unreserve(curator, bounty.curator_deposit);
									debug_assert!(err_amount.is_zero());
									bounty.curator_deposit = Zero::zero();
									// Continue to change bounty status below...
								}
							},
						}
					},
					BountyStatus::PendingPayout { ref curator, .. } => {
						// The bounty is pending payout, so only council can unassign a curator.
						// By doing so, they are claiming the curator is acting maliciously, so
						// we slash the curator.
						ensure!(maybe_sender.is_none(), BadOrigin);
						slash_curator(curator, &mut bounty.curator_deposit);
						// Continue to change bounty status below...
					},
				};

				bounty.status = BountyStatus::Funded;
				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::CuratorUnassigned { bounty_id });
			Ok(())
		}

		/// Accept the curator role for a bounty.
		/// A deposit will be reserved from curator and refund upon successful payout.
		///
		/// May only be called from the curator.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(4)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::accept_curator())]
		pub fn accept_curator(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;

			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;

				match bounty.status {
					BountyStatus::CuratorProposed { ref curator } => {
						ensure!(signer == *curator, Error::<T, I>::RequireCurator);

						let deposit = Self::calculate_curator_deposit(&bounty.fee);
						T::Currency::reserve(curator, deposit)?;
						bounty.curator_deposit = deposit;

						let update_due = Self::treasury_block_number()
							.saturating_add(T::BountyUpdatePeriod::get());
						bounty.status =
							BountyStatus::Active { curator: curator.clone(), update_due };

						Self::deposit_event(Event::<T, I>::CuratorAccepted {
							bounty_id,
							curator: signer,
						});
						Ok(())
					},
					_ => Err(Error::<T, I>::UnexpectedStatus.into()),
				}
			})?;
			Ok(())
		}

		/// Award bounty to a beneficiary account. The beneficiary will be able to claim the funds
		/// after a delay.
		///
		/// The dispatch origin for this call must be the curator of this bounty.
		///
		/// - `bounty_id`: Bounty ID to award.
		/// - `beneficiary`: The beneficiary account whom will receive the payout.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(5)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::award_bounty())]
		pub fn award_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
			beneficiary: AccountIdLookupOf<T>,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;
			let beneficiary = T::Lookup::lookup(beneficiary)?;

			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;

				// Ensure no active child bounties before processing the call.
				ensure!(
					T::ChildBountyManager::child_bounties_count(bounty_id) == 0,
					Error::<T, I>::HasActiveChildBounty
				);

				match &bounty.status {
					BountyStatus::Active { curator, .. } => {
						ensure!(signer == *curator, Error::<T, I>::RequireCurator);
					},
					_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
				}
				bounty.status = BountyStatus::PendingPayout {
					curator: signer,
					beneficiary: beneficiary.clone(),
					unlock_at: Self::treasury_block_number() + T::BountyDepositPayoutDelay::get(),
				};

				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::BountyAwarded { index: bounty_id, beneficiary });
			Ok(())
		}

		/// Claim the payout from an awarded bounty after payout delay.
		///
		/// The dispatch origin for this call must be the beneficiary of this bounty.
		///
		/// - `bounty_id`: Bounty ID to claim.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(6)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::claim_bounty())]
		pub fn claim_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResult {
			ensure_signed(origin)?; // anyone can trigger claim

			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.take().ok_or(Error::<T, I>::InvalidIndex)?;
				if let BountyStatus::PendingPayout { curator, beneficiary, unlock_at } =
					bounty.status
				{
					ensure!(Self::treasury_block_number() >= unlock_at, Error::<T, I>::Premature);
					let bounty_account = Self::bounty_account_id(bounty_id);
					let balance = T::Currency::free_balance(&bounty_account);
					let fee = bounty.fee.min(balance); // just to be safe
					let payout = balance.saturating_sub(fee);
					let err_amount = T::Currency::unreserve(&curator, bounty.curator_deposit);
					debug_assert!(err_amount.is_zero());

					// Get total child bounties curator fees, and subtract it from the parent
					// curator fee (the fee in present referenced bounty, `self`).
					let children_fee = T::ChildBountyManager::children_curator_fees(bounty_id);
					debug_assert!(children_fee <= fee);

					let final_fee = fee.saturating_sub(children_fee);
					let res =
						T::Currency::transfer(&bounty_account, &curator, final_fee, AllowDeath); // should not fail
					debug_assert!(res.is_ok());
					let res =
						T::Currency::transfer(&bounty_account, &beneficiary, payout, AllowDeath); // should not fail
					debug_assert!(res.is_ok());

					*maybe_bounty = None;

					BountyDescriptions::<T, I>::remove(bounty_id);
					T::ChildBountyManager::bounty_removed(bounty_id);

					Self::deposit_event(Event::<T, I>::BountyClaimed {
						index: bounty_id,
						payout,
						beneficiary,
					});
					Ok(())
				} else {
					Err(Error::<T, I>::UnexpectedStatus.into())
				}
			})?;
			Ok(())
		}

		/// Cancel a proposed or active bounty. All the funds will be sent to treasury and
		/// the curator deposit will be unreserved if possible.
		///
		/// Only `T::RejectOrigin` is able to cancel a bounty.
		///
		/// - `bounty_id`: Bounty ID to cancel.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(7)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::close_bounty_proposed()
			.max(<T as Config<I>>::WeightInfo::close_bounty_active()))]
		pub fn close_bounty(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResultWithPostInfo {
			T::RejectOrigin::ensure_origin(origin)?;

			Bounties::<T, I>::try_mutate_exists(
				bounty_id,
				|maybe_bounty| -> DispatchResultWithPostInfo {
					let bounty = maybe_bounty.as_ref().ok_or(Error::<T, I>::InvalidIndex)?;

					// Ensure no active child bounties before processing the call.
					ensure!(
						T::ChildBountyManager::child_bounties_count(bounty_id) == 0,
						Error::<T, I>::HasActiveChildBounty
					);

					match &bounty.status {
						BountyStatus::Proposed => {
							// The reject origin would like to cancel a proposed bounty.
							BountyDescriptions::<T, I>::remove(bounty_id);
							let value = bounty.bond;
							let imbalance = T::Currency::slash_reserved(&bounty.proposer, value).0;
							T::OnSlash::on_unbalanced(imbalance);
							*maybe_bounty = None;

							Self::deposit_event(Event::<T, I>::BountyRejected {
								index: bounty_id,
								bond: value,
							});
							// Return early, nothing else to do.
							return Ok(
								Some(<T as Config<I>>::WeightInfo::close_bounty_proposed()).into()
							);
						},
						BountyStatus::Approved | BountyStatus::ApprovedWithCurator { .. } => {
							// For weight reasons, we don't allow a council to cancel in this phase.
							// We ask for them to wait until it is funded before they can cancel.
							return Err(Error::<T, I>::UnexpectedStatus.into());
						},
						BountyStatus::Funded | BountyStatus::CuratorProposed { .. } => {
							// Nothing extra to do besides the removal of the bounty below.
						},
						BountyStatus::Active { curator, .. } => {
							// Cancelled by council, refund deposit of the working curator.
							let err_amount =
								T::Currency::unreserve(curator, bounty.curator_deposit);
							debug_assert!(err_amount.is_zero());
							// Then execute removal of the bounty below.
						},
						BountyStatus::PendingPayout { .. } => {
							// Bounty is already pending payout. If council wants to cancel
							// this bounty, it should mean the curator was acting maliciously.
							// So the council should first unassign the curator, slashing their
							// deposit.
							return Err(Error::<T, I>::PendingPayout.into());
						},
					}

					let bounty_account = Self::bounty_account_id(bounty_id);

					BountyDescriptions::<T, I>::remove(bounty_id);

					T::TransferAllAssets::force_transfer_all_assets(
						&bounty_account,
						&Self::account_id(),
					)?;

					*maybe_bounty = None;
					T::ChildBountyManager::bounty_removed(bounty_id);

					Self::deposit_event(Event::<T, I>::BountyCanceled { index: bounty_id });
					Ok(Some(<T as Config<I>>::WeightInfo::close_bounty_active()).into())
				},
			)
		}

		/// Extend the expiry time of an active bounty.
		///
		/// The dispatch origin for this call must be the curator of this bounty.
		///
		/// - `bounty_id`: Bounty ID to extend.
		/// - `remark`: additional information.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(8)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::extend_bounty_expiry())]
		pub fn extend_bounty_expiry(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
			_remark: Vec<u8>,
		) -> DispatchResult {
			let signer = ensure_signed(origin)?;

			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;

				match bounty.status {
					BountyStatus::Active { ref curator, ref mut update_due } => {
						ensure!(*curator == signer, Error::<T, I>::RequireCurator);
						*update_due = Self::treasury_block_number()
							.saturating_add(T::BountyUpdatePeriod::get())
							.max(*update_due);
					},
					_ => return Err(Error::<T, I>::UnexpectedStatus.into()),
				}

				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::BountyExtended { index: bounty_id });
			Ok(())
		}

		/// Approve bountry and propose a curator simultaneously.
		/// This call is a shortcut to calling `approve_bounty` and `propose_curator` separately.
		///
		/// May only be called from `T::SpendOrigin`.
		///
		/// - `bounty_id`: Bounty ID to approve.
		/// - `curator`: The curator account whom will manage this bounty.
		/// - `fee`: The curator fee.
		///
		/// ## Complexity
		/// - O(1).
		#[pallet::call_index(9)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::approve_bounty_with_curator())]
		pub fn approve_bounty_with_curator(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
			curator: AccountIdLookupOf<T>,
			#[pallet::compact] fee: BalanceOf<T, I>,
		) -> DispatchResult {
			let max_amount = T::SpendOrigin::ensure_origin(origin)?;
			let curator = T::Lookup::lookup(curator)?;
			Bounties::<T, I>::try_mutate_exists(bounty_id, |maybe_bounty| -> DispatchResult {
				// approve bounty
				let bounty = maybe_bounty.as_mut().ok_or(Error::<T, I>::InvalidIndex)?;
				ensure!(
					bounty.value <= max_amount,
					pallet_treasury::Error::<T, I>::InsufficientPermission
				);
				ensure!(bounty.status == BountyStatus::Proposed, Error::<T, I>::UnexpectedStatus);
				ensure!(fee < bounty.value, Error::<T, I>::InvalidFee);

				BountyApprovals::<T, I>::try_append(bounty_id)
					.map_err(|()| Error::<T, I>::TooManyQueued)?;

				bounty.status = BountyStatus::ApprovedWithCurator { curator: curator.clone() };
				bounty.fee = fee;

				Ok(())
			})?;

			Self::deposit_event(Event::<T, I>::BountyApproved { index: bounty_id });
			Self::deposit_event(Event::<T, I>::CuratorProposed { bounty_id, curator });

			Ok(())
		}

		/// Poke the deposit reserved for creating a bounty proposal.
		///
		/// This can be used by accounts to update their reserved amount.
		///
		/// The dispatch origin for this call must be _Signed_.
		///
		/// Parameters:
		/// - `bounty_id`: The bounty id for which to adjust the deposit.
		///
		/// If the deposit is updated, the difference will be reserved/unreserved from the
		/// proposer's account.
		///
		/// The transaction is made free if the deposit is updated and paid otherwise.
		///
		/// Emits `DepositPoked` if the deposit is updated.
		#[pallet::call_index(10)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::poke_deposit())]
		pub fn poke_deposit(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			let deposit_updated = Self::poke_bounty_deposit(bounty_id)?;

			Ok(if deposit_updated { Pays::No } else { Pays::Yes }.into())
		}

		/// Reclaim funds stranded in a closed bounty's account back to the treasury.
		///
		/// Permissionless. Moves all remaining assets from a closed bounty's account back to the
		/// treasury in a single call. Which assets are swept depends on the `TransferAllAssets`
		/// configuration.
		///
		/// The call is free if funds were reclaimed and paid otherwise, so no-op calls cannot be
		/// used to grief the network. Emits `BountyFundsReclaimed` on success.
		///
		/// ## Complexity
		/// - O(A) where A is the number of relevant assets configured in `TransferAllAssets`.
		#[pallet::call_index(11)]
		#[pallet::weight(<T as Config<I>>::WeightInfo::reclaim_bounty_funds())]
		pub fn reclaim_bounty_funds(
			origin: OriginFor<T>,
			#[pallet::compact] bounty_id: BountyIndex,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			// A live bounty still manages its account, so leave it untouched.
			ensure!(!Bounties::<T, I>::contains_key(bounty_id), Error::<T, I>::BountyStillActive);

			debug_assert!(
				T::ChildBountyManager::child_bounties_count(bounty_id) == 0,
				"child bounties should not exist for a closed bounty"
			);

			let bounty_account = Self::bounty_account_id(bounty_id);
			let treasury_account = Self::account_id();

			let transferred = T::TransferAllAssets::force_transfer_all_assets(
				&bounty_account,
				&treasury_account,
			)?;

			// Free only if something moved, otherwise paid to prevent griefing.
			if !transferred {
				return Ok(Pays::Yes.into());
			}

			Self::deposit_event(Event::<T, I>::BountyFundsReclaimed { bounty_id });

			Ok(Pays::No.into())
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

		Ok(())
	}

	/// # Invariants
	///
	/// * `BountyCount` should be greater or equals to the length of the number of items in
	///   `Bounties`.
	/// * `BountyCount` should be greater or equals to the length of the number of items in
	///   `BountyDescriptions`.
	/// * Number of items in `Bounties` should be the same as `BountyDescriptions` length.
	fn try_state_bounties_count() -> Result<(), sp_runtime::TryRuntimeError> {
		let bounties_length = Bounties::<T, I>::iter().count() as u32;

		ensure!(
			<BountyCount<T, I>>::get() >= bounties_length,
			"`BountyCount` must be grater or equals the number of `Bounties` in storage"
		);

		let bounties_description_length = BountyDescriptions::<T, I>::iter().count() as u32;
		ensure!(
			<BountyCount<T, I>>::get() >= bounties_description_length,
			"`BountyCount` must be grater or equals the number of `BountiesDescriptions` in storage."
		);

		ensure!(
				bounties_length == bounties_description_length,
				"Number of `Bounties` in storage must be the same as the Number of `BountiesDescription` in storage."
		);
		Ok(())
	}
}

impl<T: Config<I>, I: 'static> Pallet<T, I> {
	/// Get the block number used in the treasury pallet.
	///
	/// It may be configured to use the relay chain block number on a parachain.
	pub fn treasury_block_number() -> BlockNumberFor<T, I> {
		<T as pallet_treasury::Config<I>>::BlockNumberProvider::current_block_number()
	}

	/// Calculate the deposit required for a curator.
	pub fn calculate_curator_deposit(fee: &BalanceOf<T, I>) -> BalanceOf<T, I> {
		let mut deposit = T::CuratorDepositMultiplier::get() * *fee;

		if let Some(max_deposit) = T::CuratorDepositMax::get() {
			deposit = deposit.min(max_deposit)
		}

		if let Some(min_deposit) = T::CuratorDepositMin::get() {
			deposit = deposit.max(min_deposit)
		}

		deposit
	}

	/// The account ID of the treasury pot.
	///
	/// This actually does computation. If you need to keep using it, then make sure you cache the
	/// value and only call this once.
	pub fn account_id() -> T::AccountId {
		T::PalletId::get().into_account_truncating()
	}

	/// The account ID of a bounty account
	pub fn bounty_account_id(id: BountyIndex) -> T::AccountId {
		// only use two byte prefix to support 16 byte account id (used by test)
		// "modl" ++ "py/trsry" ++ "bt" is 14 bytes, and two bytes remaining for bounty index
		T::PalletId::get().into_sub_account_truncating(("bt", id))
	}

	fn create_bounty(
		proposer: T::AccountId,
		description: Vec<u8>,
		value: BalanceOf<T, I>,
	) -> DispatchResult {
		let bounded_description: BoundedVec<_, _> =
			description.try_into().map_err(|_| Error::<T, I>::ReasonTooBig)?;
		ensure!(value >= T::BountyValueMinimum::get(), Error::<T, I>::InvalidValue);

		let index = BountyCount::<T, I>::get();

		// reserve deposit for new bounty
		let bond = Self::calculate_bounty_deposit(&bounded_description);
		T::Currency::reserve(&proposer, bond)
			.map_err(|_| Error::<T, I>::InsufficientProposersBalance)?;

		BountyCount::<T, I>::put(index + 1);

		let bounty = Bounty {
			proposer,
			value,
			fee: 0u32.into(),
			curator_deposit: 0u32.into(),
			bond,
			status: BountyStatus::Proposed,
		};

		Bounties::<T, I>::insert(index, &bounty);
		BountyDescriptions::<T, I>::insert(index, bounded_description);

		Self::deposit_event(Event::<T, I>::BountyProposed { index });

		Ok(())
	}

	/// Helper function to calculate the bounty storage deposit.
	fn calculate_bounty_deposit(
		description: &BoundedVec<u8, T::MaximumReasonLength>,
	) -> BalanceOf<T, I> {
		T::BountyDepositBase::get().saturating_add(
			T::DataDepositPerByte::get().saturating_mul((description.len() as u32).into()),
		)
	}

	/// Helper function to poke the deposit reserved for proposing a bounty.
	///
	/// Returns true if the deposit was updated and false otherwise.
	fn poke_bounty_deposit(bounty_id: BountyIndex) -> Result<bool, DispatchError> {
		let mut bounty = Bounties::<T, I>::get(bounty_id).ok_or(Error::<T, I>::InvalidIndex)?;
		let bounty_description =
			BountyDescriptions::<T, I>::get(bounty_id).ok_or(Error::<T, I>::InvalidIndex)?;
		// ensure that the bounty status is proposed.
		ensure!(bounty.status == BountyStatus::Proposed, Error::<T, I>::UnexpectedStatus);

		let new_bond = Self::calculate_bounty_deposit(&bounty_description);
		let old_bond = bounty.bond;
		if new_bond == old_bond {
			return Ok(false);
		}
		if new_bond > old_bond {
			let extra = new_bond.saturating_sub(old_bond);
			T::Currency::reserve(&bounty.proposer, extra)?;
		} else {
			let excess = old_bond.saturating_sub(new_bond);
			let remaining_unreserved = T::Currency::unreserve(&bounty.proposer, excess);
			if !remaining_unreserved.is_zero() {
				defensive!(
					"Failed to unreserve full amount. (Requested, Actual)",
					(excess, excess.saturating_sub(remaining_unreserved))
				);
			}
		}
		bounty.bond = new_bond;
		Bounties::<T, I>::insert(bounty_id, &bounty);

		Self::deposit_event(Event::<T, I>::DepositPoked {
			bounty_id,
			proposer: bounty.proposer,
			old_deposit: old_bond,
			new_deposit: new_bond,
		});

		Ok(true)
	}
}

impl<T: Config<I>, I: 'static> pallet_treasury::SpendFunds<T, I> for Pallet<T, I> {
	fn spend_funds(
		budget_remaining: &mut BalanceOf<T, I>,
		imbalance: &mut PositiveImbalanceOf<T, I>,
		total_weight: &mut Weight,
		missed_any: &mut bool,
	) {
		let bounties_len = BountyApprovals::<T, I>::mutate(|v| {
			let bounties_approval_len = v.len() as u32;
			v.retain(|&index| {
				Bounties::<T, I>::mutate(index, |bounty| {
					// Should always be true, but shouldn't panic if false or we're screwed.
					if let Some(bounty) = bounty {
						if bounty.value <= *budget_remaining {
							*budget_remaining -= bounty.value;

							// jump through the funded phase if we're already approved with curator
							if let BountyStatus::ApprovedWithCurator { curator } = &bounty.status {
								bounty.status =
									BountyStatus::CuratorProposed { curator: curator.clone() };
							} else {
								bounty.status = BountyStatus::Funded;
							}

							// return their deposit.
							let err_amount = T::Currency::unreserve(&bounty.proposer, bounty.bond);
							debug_assert!(err_amount.is_zero());

							// fund the bounty account
							imbalance.subsume(T::Currency::deposit_creating(
								&Self::bounty_account_id(index),
								bounty.value,
							));

							Self::deposit_event(Event::<T, I>::BountyBecameActive { index });
							false
						} else {
							*missed_any = true;
							true
						}
					} else {
						false
					}
				})
			});
			bounties_approval_len
		});

		*total_weight += <T as pallet::Config<I>>::WeightInfo::spend_funds(bounties_len);
	}
}

// Default impl for when ChildBounties is not being used in the runtime.
impl<Balance: Zero> ChildBountyManager<Balance> for () {
	fn child_bounties_count(_bounty_id: BountyIndex) -> BountyIndex {
		Default::default()
	}

	fn children_curator_fees(_bounty_id: BountyIndex) -> Balance {
		Zero::zero()
	}

	fn bounty_removed(_bounty_id: BountyIndex) {}
}
