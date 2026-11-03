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

//! Additional logic for the Core Fellowship. This determines salary, registers activity/passivity
//! and handles promotion and demotion periods.
//!
//! This only handles members of non-zero rank.
//!
//! # Process Flow
//!
//! - Begin with a call to `induct`, where some privileged origin (perhaps a pre-existing member of
//!   `rank > 1`) is able to make a candidate from an account and introduce it to be tracked in this
//!   pallet in order to allow evidence to be submitted and promotion voted on.
//! - The candidate then calls `submit_evidence` to apply for their promotion to rank 1.
//! - A `PromoteOrigin` of at least rank 1 calls `promote` on the candidate to elevate it to rank 1.
//! - Some time later but before rank 1's `demotion_period` elapses, candidate calls
//!   `submit_evidence` with evidence of their efforts to apply for approval to stay at rank 1.
//! - An `ApproveOrigin` of at least rank 1 calls `approve` on the candidate to avoid imminent
//!   demotion and keep it at rank 1.
//! - These last two steps continue until the candidate is ready to apply for a promotion, at which
//!   point the previous two steps are repeated with a higher rank.
//! - If the member fails to get an approval within the `demotion_period` then anyone may call
//!   `bump` to demote the candidate by one rank.
//! - If a candidate fails to be promoted to a member within the `offboard_timeout` period, then
//!   anyone may call `bump` to remove the account's candidacy.
//! - Pre-existing members may call `import_member` on themselves (formerly `import`) to have their
//!   rank recognised and be inducted into this pallet (to gain a salary and allow for eventual
//!   promotion).
//! - If, externally to this pallet, a member or candidate has their rank removed completely, then
//!   `offboard` may be called to remove them entirely from this pallet.
//!
//! Note there is a difference between having a rank of 0 (whereby the account is a *candidate*) and
//! having no rank at all (whereby we consider it *unranked*). An account can be demoted from rank
//! 0 to become unranked. This process is called being offboarded and there is an extrinsic to do
//! this explicitly when external factors to this pallet have caused the tracked account to become
//! unranked. At rank 0, there is not a "demotion" period after which the account may be bumped to
//! become offboarded but rather an "offboard timeout".
//!
//! Candidates may be introduced (i.e. an account to go from unranked to rank of 0) by an origin
//! of a different privilege to that for promotion. This allows the possibility for even a single
//! existing member to introduce a new candidate without payment.
//!
//! Only tracked/ranked accounts may submit evidence for their proof and promotion. Candidates
//! cannot be approved - they must proceed only to promotion prior to the offboard timeout elapsing.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::{fmt::Debug, marker::PhantomData};
use scale_info::TypeInfo;
use sp_arithmetic::traits::{Saturating, Zero};

use frame_support::{
	defensive,
	dispatch::DispatchResultWithPostInfo,
	ensure, impl_ensure_origin_with_arg_ignoring_arg,
	traits::{
		tokens::Balance as BalanceTrait, EnsureOrigin, EnsureOriginWithArg, Get, RankedMembers,
		RankedMembersSwapHandler,
	},
	BoundedVec, CloneNoBound, DebugNoBound, EqNoBound, PartialEqNoBound,
};

#[cfg(test)]
mod tests;

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
pub mod migration;
pub mod weights;

pub use pallet::*;
pub use weights::*;

/// The desired outcome for which evidence is presented.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Eq,
	PartialEq,
	Copy,
	Clone,
	TypeInfo,
	MaxEncodedLen,
	Debug,
)]
pub enum Wish {
	/// Member wishes only to retain their current rank.
	Retention,
	/// Member wishes to be promoted.
	Promotion,
}

/// A piece of evidence to underpin a [Wish].
///
/// From the pallet's perspective, this is just a blob of data without meaning. The fellows can
/// decide how to concretely utilise it. This could be an IPFS hash, a URL or structured data.
pub type Evidence<T, I> = BoundedVec<u8, <T as Config<I>>::EvidenceSize>;

/// The status of the pallet instance.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	CloneNoBound,
	EqNoBound,
	PartialEqNoBound,
	DebugNoBound,
	TypeInfo,
	MaxEncodedLen,
)]
#[scale_info(skip_type_params(Ranks))]
pub struct ParamsType<
	Balance: Clone + Eq + PartialEq + Debug,
	BlockNumber: Clone + Eq + PartialEq + Debug,
	Ranks: Get<u32>,
> {
	/// The amounts to be paid when a member of a given rank (-1) is active.
	pub active_salary: BoundedVec<Balance, Ranks>,
	/// The amounts to be paid when a member of a given rank (-1) is passive.
	pub passive_salary: BoundedVec<Balance, Ranks>,
	/// The period between which unproven members become demoted.
	pub demotion_period: BoundedVec<BlockNumber, Ranks>,
	/// The period between which members must wait before they may proceed to this rank.
	pub min_promotion_period: BoundedVec<BlockNumber, Ranks>,
	/// Amount by which an account can remain at rank 0 (candidate before being offboard entirely).
	pub offboard_timeout: BlockNumber,
}

impl<
		Balance: Default + Copy + Eq + Debug,
		BlockNumber: Default + Copy + Eq + Debug,
		Ranks: Get<u32>,
	> Default for ParamsType<Balance, BlockNumber, Ranks>
{
	fn default() -> Self {
		Self {
			active_salary: Default::default(),
			passive_salary: Default::default(),
			demotion_period: Default::default(),
			min_promotion_period: Default::default(),
			offboard_timeout: BlockNumber::default(),
		}
	}
}

pub struct ConvertU16ToU32<Inner>(PhantomData<Inner>);
impl<Inner: Get<u16>> Get<u32> for ConvertU16ToU32<Inner> {
	fn get() -> u32 {
		Inner::get() as u32
	}
}

/// The status of a single member.
#[derive(Encode, Decode, Eq, PartialEq, Clone, TypeInfo, MaxEncodedLen, Debug)]
pub struct MemberStatus<BlockNumber> {
	/// Are they currently active?
	pub is_active: bool,
	/// The block number at which we last promoted them.
	pub last_promotion: BlockNumber,
	/// The last time a member was demoted, promoted or proved their rank.
	pub last_proof: BlockNumber,
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::{
		dispatch::Pays,
		pallet_prelude::*,
		traits::{tokens::GetSalary, EnsureOrigin},
	};
	use frame_system::{ensure_root, pallet_prelude::*};
	use sp_runtime::traits::BlockNumberProvider;
	/// The in-code storage version.
	const STORAGE_VERSION: StorageVersion = StorageVersion::new(2);

	#[pallet::pallet]
	#[pallet::storage_version(STORAGE_VERSION)]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config {
		/// Weight information for extrinsics in this pallet.
		type WeightInfo: WeightInfo;

		/// The runtime event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The current membership of the fellowship.
		type Members: RankedMembers<
			AccountId = <Self as frame_system::Config>::AccountId,
			Rank = u16,
		>;

		/// The type in which salaries/budgets are measured.
		type Balance: BalanceTrait;

		/// The origin which has permission update the parameters.
		type ParamsOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The origin which has permission to move a candidate into being tracked in this pallet.
		/// Generally a very low-permission, such as a pre-existing member of rank 1 or above.
		///
		/// This allows the candidate to deposit evidence for their request to be promoted to a
		/// member.
		type InductOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The origin which has permission to issue a proof that a member may retain their rank.
		/// The `Success` value is the maximum rank of members it is able to prove.
		type ApproveOrigin: EnsureOrigin<Self::RuntimeOrigin, Success = RankOf<Self, I>>;

		/// The origin which has permission to promote a member. The `Success` value is the maximum
		/// rank to which it can promote.
		type PromoteOrigin: EnsureOrigin<Self::RuntimeOrigin, Success = RankOf<Self, I>>;

		/// The origin that has permission to "fast" promote a member by ignoring promotion periods
		/// and skipping ranks. The `Success` value is the maximum rank to which it can promote.
		type FastPromoteOrigin: EnsureOrigin<Self::RuntimeOrigin, Success = RankOf<Self, I>>;

		/// The maximum size in bytes submitted evidence is allowed to be.
		#[pallet::constant]
		type EvidenceSize: Get<u32>;

		/// Represents the highest possible rank in this pallet.
		///
		/// Increasing this value is supported, but decreasing it may lead to a broken state.
		#[pallet::constant]
		type MaxRank: Get<u16>;

		/// Provides the current block number.
		///
		/// This is usually `cumulus_pallet_parachain_system::RelaychainDataProvider` if a
		/// parachain, or `frame_system::Pallet` if a solo- or relaychain.
		type BlockNumberProvider: BlockNumberProvider;
	}

	pub type BlockNumberFor<T, I = ()> =
		<<T as Config<I>>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;
	pub type ParamsOf<T, I> = ParamsType<
		<T as Config<I>>::Balance,
		BlockNumberFor<T, I>,
		ConvertU16ToU32<<T as Config<I>>::MaxRank>,
	>;
	pub type PartialParamsOf<T, I> = ParamsType<
		Option<<T as Config<I>>::Balance>,
		Option<BlockNumberFor<T, I>>,
		ConvertU16ToU32<<T as Config<I>>::MaxRank>,
	>;
	pub type MemberStatusOf<T, I> = MemberStatus<BlockNumberFor<T, I>>;
	pub type RankOf<T, I> = <<T as Config<I>>::Members as RankedMembers>::Rank;

	/// The overall status of the system.
	#[pallet::storage]
	pub type Params<T: Config<I>, I: 'static = ()> = StorageValue<_, ParamsOf<T, I>, ValueQuery>;

	/// The status of a claimant.
	#[pallet::storage]
	pub type Member<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, T::AccountId, MemberStatusOf<T, I>, OptionQuery>;

	/// Some evidence together with the desired outcome for which it was presented.
	#[pallet::storage]
	pub type MemberEvidence<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Twox64Concat, T::AccountId, (Wish, Evidence<T, I>), OptionQuery>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// Parameters for the pallet have changed.
		ParamsChanged { params: ParamsOf<T, I> },
		/// Member activity flag has been set.
		ActiveChanged { who: T::AccountId, is_active: bool },
		/// Member has begun being tracked in this pallet.
		Inducted { who: T::AccountId },
		/// Member has been removed from being tracked in this pallet (i.e. because rank is now
		/// zero).
		Offboarded { who: T::AccountId },
		/// Member has been promoted to the given rank.
		Promoted { who: T::AccountId, to_rank: RankOf<T, I> },
		/// Member has been demoted to the given (non-zero) rank.
		Demoted { who: T::AccountId, to_rank: RankOf<T, I> },
		/// Member has been proven at their current rank, postponing auto-demotion.
		Proven { who: T::AccountId, at_rank: RankOf<T, I> },
		/// Member has stated evidence of their efforts their request for rank.
		Requested { who: T::AccountId, wish: Wish },
		/// Some submitted evidence was judged and removed. There may or may not have been a change
		/// to the rank, but in any case, `last_proof` is reset.
		EvidenceJudged {
			/// The member/candidate.
			who: T::AccountId,
			/// The desired outcome for which the evidence was presented.
			wish: Wish,
			/// The evidence of efforts.
			evidence: Evidence<T, I>,
			/// The old rank, prior to this change.
			old_rank: u16,
			/// New rank. If `None` then candidate record was removed entirely.
			new_rank: Option<u16>,
		},
		/// Pre-ranked account has been inducted at their current rank.
		Imported { who: T::AccountId, rank: RankOf<T, I> },
		/// A member had its AccountId swapped.
		Swapped { who: T::AccountId, new_who: T::AccountId },
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Member's rank is too low.
		Unranked,
		/// Member's rank is not zero.
		Ranked,
		/// Member's rank is not as expected - generally means that the rank provided to the call
		/// does not agree with the state of the system.
		UnexpectedRank,
		/// The given rank is invalid - this generally means it's not between 1 and `RANK_COUNT`.
		InvalidRank,
		/// The origin does not have enough permission to do this operation.
		NoPermission,
		/// No work needs to be done at present for this member.
		NothingDoing,
		/// The candidate has already been inducted. This should never happen since it would
		/// require a candidate (rank 0) to already be tracked in the pallet.
		AlreadyInducted,
		/// The candidate has not been inducted, so cannot be offboarded from this pallet.
		NotTracked,
		/// Operation cannot be done yet since not enough time has passed.
		TooSoon,
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Bump the state of a member.
		///
		/// This will demote a member whose `last_proof` is now beyond their rank's
		/// `demotion_period`.
		///
		/// - `origin`: A `Signed` origin of an account.
		/// - `who`: A member account whose state is to be updated.
		#[pallet::weight(T::WeightInfo::bump_offboard().max(T::WeightInfo::bump_demote()))]
		#[pallet::call_index(0)]
		pub fn bump(origin: OriginFor<T>, who: T::AccountId) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			let mut member = Member::<T, I>::get(&who).ok_or(Error::<T, I>::NotTracked)?;
			let rank = T::Members::rank_of(&who).ok_or(Error::<T, I>::Unranked)?;

			let params = Params::<T, I>::get();
			let demotion_period = if rank == 0 {
				params.offboard_timeout
			} else {
				let rank_index = Self::rank_to_index(rank).ok_or(Error::<T, I>::InvalidRank)?;
				params.demotion_period[rank_index]
			};

			if demotion_period.is_zero() {
				return Err(Error::<T, I>::NothingDoing.into());
			}

			let demotion_block = member.last_proof.saturating_add(demotion_period);

			// Ensure enough time has passed.
			let now = T::BlockNumberProvider::current_block_number();
			if now >= demotion_block {
				T::Members::demote(&who)?;
				let maybe_to_rank = T::Members::rank_of(&who);
				Self::dispose_evidence(who.clone(), rank, maybe_to_rank);
				let event = if let Some(to_rank) = maybe_to_rank {
					member.last_proof = now;
					Member::<T, I>::insert(&who, &member);
					Event::<T, I>::Demoted { who, to_rank }
				} else {
					Member::<T, I>::remove(&who);
					Event::<T, I>::Offboarded { who }
				};
				Self::deposit_event(event);
				return Ok(Pays::No.into());
			}

			Err(Error::<T, I>::NothingDoing.into())
		}

		/// Set the parameters.
		///
		/// - `origin`: An origin complying with `ParamsOrigin` or root.
		/// - `params`: The new parameters for the pallet.
		#[pallet::weight(T::WeightInfo::set_params())]
		#[pallet::call_index(1)]
		pub fn set_params(origin: OriginFor<T>, params: Box<ParamsOf<T, I>>) -> DispatchResult {
			T::ParamsOrigin::ensure_origin_or_root(origin)?;

			Params::<T, I>::put(params.as_ref());
			Self::deposit_event(Event::<T, I>::ParamsChanged { params: *params });

			Ok(())
		}

		/// Set whether a member is active or not.
		///
		/// - `origin`: A `Signed` origin of a member's account.
		/// - `is_active`: `true` iff the member is active.
		#[pallet::weight(T::WeightInfo::set_active())]
		#[pallet::call_index(2)]
		pub fn set_active(origin: OriginFor<T>, is_active: bool) -> DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(
				T::Members::rank_of(&who).map_or(false, |r| !r.is_zero()),
				Error::<T, I>::Unranked
			);
			let mut member = Member::<T, I>::get(&who).ok_or(Error::<T, I>::NotTracked)?;
			member.is_active = is_active;
			Member::<T, I>::insert(&who, &member);
			Self::deposit_event(Event::<T, I>::ActiveChanged { who, is_active });
			Ok(())
		}

		/// Approve a member to continue at their rank.
		///
		/// This resets `last_proof` to the current block, thereby delaying any automatic demotion.
		///
		/// `who` must already be tracked by this pallet for this to have an effect.
		///
		/// - `origin`: An origin which satisfies `ApproveOrigin` or root.
		/// - `who`: A member (i.e. of non-zero rank).
		/// - `at_rank`: The rank of member.
		#[pallet::weight(T::WeightInfo::approve())]
		#[pallet::call_index(3)]
		pub fn approve(
			origin: OriginFor<T>,
			who: T::AccountId,
			at_rank: RankOf<T, I>,
		) -> DispatchResult {
			match T::ApproveOrigin::try_origin(origin) {
				Ok(allow_rank) => ensure!(allow_rank >= at_rank, Error::<T, I>::NoPermission),
				Err(origin) => ensure_root(origin)?,
			}
			ensure!(at_rank > 0, Error::<T, I>::InvalidRank);
			let rank = T::Members::rank_of(&who).ok_or(Error::<T, I>::Unranked)?;
			ensure!(rank == at_rank, Error::<T, I>::UnexpectedRank);
			let mut member = Member::<T, I>::get(&who).ok_or(Error::<T, I>::NotTracked)?;

			member.last_proof = T::BlockNumberProvider::current_block_number();
			Member::<T, I>::insert(&who, &member);

			Self::dispose_evidence(who.clone(), at_rank, Some(at_rank));
			Self::deposit_event(Event::<T, I>::Proven { who, at_rank });

			Ok(())
		}

		/// Introduce a new and unranked candidate (rank zero).
		///
		/// - `origin`: An origin which satisfies `InductOrigin` or root.
		/// - `who`: The account ID of the candidate to be inducted and become a member.
		#[pallet::weight(T::WeightInfo::induct())]
		#[pallet::call_index(4)]
		pub fn induct(origin: OriginFor<T>, who: T::AccountId) -> DispatchResult {
			match T::InductOrigin::try_origin(origin) {
				Ok(_) => {},
				Err(origin) => ensure_root(origin)?,
			}
			ensure!(!Member::<T, I>::contains_key(&who), Error::<T, I>::AlreadyInducted);
			ensure!(T::Members::rank_of(&who).is_none(), Error::<T, I>::Ranked);

			T::Members::induct(&who)?;
			let now = T::BlockNumberProvider::current_block_number();
			Member::<T, I>::insert(
				&who,
				MemberStatus { is_active: true, last_promotion: now, last_proof: now },
			);
			Self::deposit_event(Event::<T, I>::Inducted { who });
			Ok(())
		}

		/// Increment the rank of a ranked and tracked account.
		///
		/// - `origin`: An origin which satisfies `PromoteOrigin` with a `Success` result of
		///   `to_rank` or more or root.
		/// - `who`: The account ID of the member to be promoted to `to_rank`.
		/// - `to_rank`: One more than the current rank of `who`.
		#[pallet::weight(T::WeightInfo::promote())]
		#[pallet::call_index(5)]
		pub fn promote(
			origin: OriginFor<T>,
			who: T::AccountId,
			to_rank: RankOf<T, I>,
		) -> DispatchResult {
			match T::PromoteOrigin::try_origin(origin) {
				Ok(allow_rank) => ensure!(allow_rank >= to_rank, Error::<T, I>::NoPermission),
				Err(origin) => ensure_root(origin)?,
			}
			let rank = T::Members::rank_of(&who).ok_or(Error::<T, I>::Unranked)?;
			ensure!(
				rank.checked_add(1).map_or(false, |i| i == to_rank),
				Error::<T, I>::UnexpectedRank
			);

			let mut member = Member::<T, I>::get(&who).ok_or(Error::<T, I>::NotTracked)?;
			let now = T::BlockNumberProvider::current_block_number();

			let params = Params::<T, I>::get();
			let rank_index = Self::rank_to_index(to_rank).ok_or(Error::<T, I>::InvalidRank)?;
			let min_period = params.min_promotion_period[rank_index];
			// Ensure enough time has passed.
			ensure!(
				member.last_promotion.saturating_add(min_period) <= now,
				Error::<T, I>::TooSoon,
			);

			T::Members::promote(&who)?;
			member.last_promotion = now;
			member.last_proof = now;
			Member::<T, I>::insert(&who, &member);
			Self::dispose_evidence(who.clone(), rank, Some(to_rank));

			Self::deposit_event(Event::<T, I>::Promoted { who, to_rank });

			Ok(())
		}

		/// Fast promotions can skip ranks and ignore the `min_promotion_period`.
		///
		/// This is useful for out-of-band promotions, hence it has its own `FastPromoteOrigin` to
		/// be (possibly) more restrictive than `PromoteOrigin`. Note that the member must already
		/// be inducted.
		#[pallet::weight(T::WeightInfo::promote_fast(*to_rank as u32))]
		#[pallet::call_index(10)]
		pub fn promote_fast(
			origin: OriginFor<T>,
			who: T::AccountId,
			to_rank: RankOf<T, I>,
		) -> DispatchResult {
			match T::FastPromoteOrigin::try_origin(origin) {
				Ok(allow_rank) => ensure!(allow_rank >= to_rank, Error::<T, I>::NoPermission),
				Err(origin) => ensure_root(origin)?,
			}
			ensure!(to_rank <= T::MaxRank::get(), Error::<T, I>::InvalidRank);
			let curr_rank = T::Members::rank_of(&who).ok_or(Error::<T, I>::Unranked)?;
			ensure!(to_rank > curr_rank, Error::<T, I>::UnexpectedRank);

			let mut member = Member::<T, I>::get(&who).ok_or(Error::<T, I>::NotTracked)?;
			let now = T::BlockNumberProvider::current_block_number();
			member.last_promotion = now;
			member.last_proof = now;

			for rank in (curr_rank + 1)..=to_rank {
				T::Members::promote(&who)?;

				// NOTE: We could factor this out, but it would destroy our invariants:
				Member::<T, I>::insert(&who, &member);

				Self::dispose_evidence(who.clone(), rank.saturating_sub(1), Some(rank));
				Self::deposit_event(Event::<T, I>::Promoted { who: who.clone(), to_rank: rank });
			}

			Ok(())
		}

		/// Stop tracking a prior member who is now not a ranked member of the collective.
		///
		/// - `origin`: A `Signed` origin of an account.
		/// - `who`: The ID of an account which was tracked in this pallet but which is now not a
		///   ranked member of the collective.
		#[pallet::weight(T::WeightInfo::offboard())]
		#[pallet::call_index(6)]
		pub fn offboard(origin: OriginFor<T>, who: T::AccountId) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			ensure!(T::Members::rank_of(&who).is_none(), Error::<T, I>::Ranked);
			ensure!(Member::<T, I>::contains_key(&who), Error::<T, I>::NotTracked);
			Member::<T, I>::remove(&who);
			MemberEvidence::<T, I>::remove(&who);
			Self::deposit_event(Event::<T, I>::Offboarded { who });
			Ok(Pays::No.into())
		}

		/// Provide evidence that a rank is deserved.
		///
		/// This is free as long as no evidence for the forthcoming judgement is already submitted.
		/// Evidence is cleared after an outcome (either demotion, promotion of approval).
		///
		/// - `origin`: A `Signed` origin of an inducted and ranked account.
		/// - `wish`: The stated desire of the member.
		/// - `evidence`: A dump of evidence to be considered. This should generally be either a
		///   Markdown-encoded document or a series of 32-byte hashes which can be found on a
		///   decentralised content-based-indexing system such as IPFS.
		#[pallet::weight(T::WeightInfo::submit_evidence())]
		#[pallet::call_index(7)]
		pub fn submit_evidence(
			origin: OriginFor<T>,
			wish: Wish,
			evidence: Evidence<T, I>,
		) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			ensure!(Member::<T, I>::contains_key(&who), Error::<T, I>::NotTracked);
			let replaced = MemberEvidence::<T, I>::contains_key(&who);
			MemberEvidence::<T, I>::insert(&who, (wish, evidence));
			Self::deposit_event(Event::<T, I>::Requested { who, wish });
			Ok(if replaced { Pays::Yes } else { Pays::No }.into())
		}

		/// Introduce an already-ranked individual of the collective into this pallet.
		///
		/// The rank may still be zero. This resets `last_proof` to the current block and
		/// `last_promotion` will be set to zero, thereby delaying any automatic demotion but
		/// allowing immediate promotion.
		///
		/// - `origin`: A signed origin of a ranked, but not tracked, account.
		#[pallet::weight(T::WeightInfo::import())]
		#[pallet::call_index(8)]
		#[deprecated = "Use `import_member` instead"]
		#[allow(deprecated)] // Otherwise FRAME will complain about using something deprecated.
		pub fn import(origin: OriginFor<T>) -> DispatchResultWithPostInfo {
			let who = ensure_signed(origin)?;
			Self::do_import(who)?;

			Ok(Pays::No.into()) // Successful imports are free
		}

		/// Introduce an already-ranked individual of the collective into this pallet.
		///
		/// The rank may still be zero. Can be called by anyone on any collective member - including
		/// the sender.
		///
		/// This resets `last_proof` to the current block and `last_promotion` will be set to zero,
		/// thereby delaying any automatic demotion but allowing immediate promotion.
		///
		/// - `origin`: A signed origin of a ranked, but not tracked, account.
		/// - `who`: The account ID of the collective member to be inducted.
		#[pallet::weight(T::WeightInfo::set_partial_params())]
		#[pallet::call_index(11)]
		pub fn import_member(
			origin: OriginFor<T>,
			who: T::AccountId,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;
			Self::do_import(who)?;

			Ok(Pays::No.into()) // Successful imports are free
		}

		/// Set the parameters partially.
		///
		/// - `origin`: An origin complying with `ParamsOrigin` or root.
		/// - `partial_params`: The new parameters for the pallet.
		///
		/// This update config with multiple arguments without duplicating
		/// the fields that does not need to update (set to None).
		#[pallet::weight(T::WeightInfo::set_partial_params())]
		#[pallet::call_index(9)]
		pub fn set_partial_params(
			origin: OriginFor<T>,
			partial_params: Box<PartialParamsOf<T, I>>,
		) -> DispatchResult {
			T::ParamsOrigin::ensure_origin_or_root(origin)?;
			let params = Params::<T, I>::mutate(|p| {
				Self::set_partial_params_slice(&mut p.active_salary, partial_params.active_salary);
				Self::set_partial_params_slice(
					&mut p.passive_salary,
					partial_params.passive_salary,
				);
				Self::set_partial_params_slice(
					&mut p.demotion_period,
					partial_params.demotion_period,
				);
				Self::set_partial_params_slice(
					&mut p.min_promotion_period,
					partial_params.min_promotion_period,
				);
				if let Some(new_offboard_timeout) = partial_params.offboard_timeout {
					p.offboard_timeout = new_offboard_timeout;
				}
				p.clone()
			});
			Self::deposit_event(Event::<T, I>::ParamsChanged { params });
			Ok(())
		}
	}

	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Partially update the base slice with a new slice
		///
		/// Only elements in the base slice which has a new value in the new slice will be updated.
		pub(crate) fn set_partial_params_slice<S>(
			base_slice: &mut BoundedVec<S, ConvertU16ToU32<T::MaxRank>>,
			new_slice: BoundedVec<Option<S>, ConvertU16ToU32<T::MaxRank>>,
		) {
			for (base_element, new_element) in base_slice.iter_mut().zip(new_slice) {
				if let Some(element) = new_element {
					*base_element = element;
				}
			}
		}

		/// Import `who` into the core-fellowship pallet.
		///
		/// `who` must be a member of the collective but *not* already imported.
		pub(crate) fn do_import(who: T::AccountId) -> DispatchResult {
			ensure!(!Member::<T, I>::contains_key(&who), Error::<T, I>::AlreadyInducted);
			let rank = T::Members::rank_of(&who).ok_or(Error::<T, I>::Unranked)?;

			let now = T::BlockNumberProvider::current_block_number();
			Member::<T, I>::insert(
				&who,
				MemberStatus { is_active: true, last_promotion: 0u32.into(), last_proof: now },
			);
			Self::deposit_event(Event::<T, I>::Imported { who, rank });

			Ok(())
		}

		/// Convert a rank into a `0..RANK_COUNT` index suitable for the arrays in Params.
		///
		/// Rank 1 becomes index 0, rank `RANK_COUNT` becomes index `RANK_COUNT - 1`. Any rank not
		/// in the range `1..=RANK_COUNT` is `None`.
		pub(crate) fn rank_to_index(rank: RankOf<T, I>) -> Option<usize> {
			if rank == 0 || rank > T::MaxRank::get() {
				None
			} else {
				Some((rank - 1) as usize)
			}
		}

		fn dispose_evidence(who: T::AccountId, old_rank: u16, new_rank: Option<u16>) {
			if let Some((wish, evidence)) = MemberEvidence::<T, I>::take(&who) {
				let e = Event::<T, I>::EvidenceJudged { who, wish, evidence, old_rank, new_rank };
				Self::deposit_event(e);
			}
		}
	}

	impl<T: Config<I>, I: 'static> GetSalary<RankOf<T, I>, T::AccountId, T::Balance> for Pallet<T, I> {
		fn get_salary(rank: RankOf<T, I>, who: &T::AccountId) -> T::Balance {
			let index = match Self::rank_to_index(rank) {
				Some(i) => i,
				None => return Zero::zero(),
			};
			let member = match Member::<T, I>::get(who) {
				Some(m) => m,
				None => return Zero::zero(),
			};
			let params = Params::<T, I>::get();
			let salary =
				if member.is_active { params.active_salary } else { params.passive_salary };
			salary[index]
		}
	}
}

/// Guard to ensure that the given origin is inducted into this pallet with a given minimum rank.
/// The account ID of the member is the `Success` value.
pub struct EnsureInducted<T, I, const MIN_RANK: u16>(PhantomData<(T, I)>);
impl<T: Config<I>, I: 'static, const MIN_RANK: u16> EnsureOrigin<T::RuntimeOrigin>
	for EnsureInducted<T, I, MIN_RANK>
{
	type Success = T::AccountId;

	fn try_origin(o: T::RuntimeOrigin) -> Result<Self::Success, T::RuntimeOrigin> {
		let who = <frame_system::EnsureSigned<_> as EnsureOrigin<_>>::try_origin(o)?;
		match T::Members::rank_of(&who) {
			Some(rank) if rank >= MIN_RANK && Member::<T, I>::contains_key(&who) => Ok(who),
			_ => Err(frame_system::RawOrigin::Signed(who).into()),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin() -> Result<T::RuntimeOrigin, ()> {
		let who = frame_benchmarking::account::<T::AccountId>("successful_origin", 0, 0);
		if T::Members::rank_of(&who).is_none() {
			T::Members::induct(&who).map_err(|_| ())?;
		}
		for _ in 0..MIN_RANK {
			if T::Members::rank_of(&who).ok_or(())? < MIN_RANK {
				T::Members::promote(&who).map_err(|_| ())?;
			}
		}
		Ok(frame_system::RawOrigin::Signed(who).into())
	}
}

impl_ensure_origin_with_arg_ignoring_arg! {
	impl< { T: Config<I>, I: 'static, const MIN_RANK: u16, A } >
		EnsureOriginWithArg<T::RuntimeOrigin, A> for EnsureInducted<T, I, MIN_RANK>
	{}
}

impl<T: Config<I>, I: 'static> RankedMembersSwapHandler<T::AccountId, u16> for Pallet<T, I> {
	fn swapped(old: &T::AccountId, new: &T::AccountId, _rank: u16) {
		if old == new {
			defensive!("Should not try to swap with self");
			return;
		}
		if !Member::<T, I>::contains_key(old) {
			defensive!("Should not try to swap non-member");
			return;
		}
		if Member::<T, I>::contains_key(new) {
			defensive!("Should not try to overwrite existing member");
			return;
		}

		if let Some(member) = Member::<T, I>::take(old) {
			Member::<T, I>::insert(new, member);
		}
		if let Some(we) = MemberEvidence::<T, I>::take(old) {
			MemberEvidence::<T, I>::insert(new, we);
		}

		Self::deposit_event(Event::<T, I>::Swapped { who: old.clone(), new_who: new.clone() });
	}
}

#[cfg(feature = "runtime-benchmarks")]
impl<T: Config<I>, I: 'static>
	pallet_ranked_collective::BenchmarkSetup<<T as frame_system::Config>::AccountId> for Pallet<T, I>
{
	fn ensure_member(who: &<T as frame_system::Config>::AccountId) {
		#[allow(deprecated)]
		Self::import(frame_system::RawOrigin::Signed(who.clone()).into()).unwrap();
	}
}
