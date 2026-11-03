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

//! The traits for dealing with a single fungible token class and any associated types.
//!
//! Also see the [`frame_tokens`] reference docs for more information about the place of
//! `fungible` traits in Substrate.
//!
//! # Available Traits
//! - [`Inspect`]: Regular balance inspector functions.
//! - [`Unbalanced`]: Low-level balance mutating functions. Does not guarantee proper book-keeping
//!   and so should not be called into directly from application code. Other traits depend on this
//!   and provide default implementations based on it.
//! - [`UnbalancedHold`]: Low-level balance mutating functions for balances placed on hold. Does not
//!   guarantee proper book-keeping and so should not be called into directly from application code.
//!   Other traits depend on this and provide default implementations based on it.
//! - [`Mutate`]: Regular balance mutator functions. Pre-implemented using [`Unbalanced`], though
//!   the `done_*` functions should likely be reimplemented in case you want to do something
//!   following the operation such as emit events.
//! - [`InspectHold`]: Inspector functions for balances on hold.
//! - [`MutateHold`]: Mutator functions for balances on hold. Mostly pre-implemented using
//!   [`UnbalancedHold`].
//! - [`InspectFreeze`]: Inspector functions for frozen balance.
//! - [`MutateFreeze`]: Mutator functions for frozen balance.
//! - [`Balanced`]: One-sided mutator functions for regular balances, which return imbalance objects
//!   which guarantee eventual book-keeping. May be useful for some sophisticated operations where
//!   funds must be removed from an account before it is known precisely what should be done with
//!   them.
//! - [`metadata::Inspect`]: Inspector functions for token metadata (name, symbol, decimals).
//! - [`metadata::Mutate`]: Mutator functions for token metadata.
//! - [`lifetime::Create`]: Trait for creating a new fungible asset.
//!
//! ## Terminology
//!
//! - **Total Issuance**: The total number of units in existence in a system.
//!
//! - **Total Balance**: The sum of an account's free and held balances.
//!
//! - **Free Balance**: A portion of an account's total balance that is not held. Note this is
//!   distinct from the Spendable Balance, which represents how much Balance the user can actually
//!   transfer.
//!
//! - **Held Balance**: Held balance still belongs to the account holder, but is suspended — it
//!   cannot be transferred or used for most operations. It may be slashed by the pallet that placed
//!   the hold.
//!
//!   Multiple holds stack rather than overlay. This means that if an account has
//!   3 holds for 100 units, the account can spend its funds for any reason down to 300 units, at
//!   which point the holds will start to come into play.
//!
//! - **Frozen Balance**: A freeze on a specified amount of an account's balance. Tokens that are
//!   frozen cannot be transferred.
//!
//!   Multiple freezes always operate over the same funds, so they "overlay" rather than
//!   "stack". This means that if an account has 3 freezes for 100 units, the account can spend its
//!   funds for any reason down to 100 units, at which point the freezes will start to come into
//!   play.
//!
//!   It's important to note that the frozen balance can exceed the total balance of the account.
//!   This is useful, eg, in cases where you want to prevent a user from transferring any fund. In
//!   such a case, setting the frozen balance to `Balance::MAX` would serve that purpose
//! effectively.
//!
//! - **Minimum Balance (a.k.a. Existential Deposit, a.k.a. ED)**: The minimum balance required to
//!   create or keep an account open. This is to prevent "dust accounts" from filling storage. When
//!   the free plus the held balance (i.e. the total balance) falls below this, then the account is
//!   said to be dead. It loses its functionality as well as any prior history and all information
//!   on it is removed from the chain's state. No account should ever have a total balance that is
//!   strictly between 0 and the existential deposit (exclusive). If this ever happens, it indicates
//!   either a bug in the implementation of this trait or an erroneous raw mutation of storage.
//!
//! - **Untouchable Balance**: The part of a user's free balance they cannot spend, due to ED or
//!   Freeze(s).
//!
//! - **Spendable Balance**: The part of a user's free balance they can actually transfer, after
//!   accounting for Holds and Freezes.
//!
//! - **Imbalance**: A condition when some funds were credited or debited without equal and opposite
//!   accounting (i.e. a difference between total issuance and account balances). Functions that
//!   result in an imbalance will return an object of the [`imbalance::Credit`] or
//!   [`imbalance::Debt`] traits that can be managed within your runtime logic.
//!
//!   If an imbalance is simply dropped, it should automatically maintain any book-keeping such as
//!   total issuance.
//!
//! ## Visualising Balance Components Together 💫
//!
//! ```ignore
//! |__total__________________________________|
//! |__on_hold__|_____________free____________|
//! |__________frozen___________|
//! |__on_hold__|__ed__|
//!             |__untouchable__|__spendable__|
//! ```
//!
//! ## Holds and Freezes
//!
//! Both holds and freezes are used to prevent an account from using some of its balance.
//!
//! The primary distinction between the two are that:
//! - Holds are cumulative (do not overlap) and are distinct from the free balance
//! - Freezes are not cumulative, and can overlap with each other or with holds
//!
//! ```ignore
//! |__total_____________________________|
//! |__hold_a__|__hold_b__|_____free_____|
//! |__on_hold____________|     // <- the sum of all holds
//! |__freeze_a_______________|
//! |__freeze_b____|
//! |__freeze_c________|
//! |__frozen_________________| // <- the max of all freezes
//! ```
//!
//! Holds are designed to be infallibly slashed, meaning that any logic using a `Freeze`
//! must handle the possibility of the frozen amount being reduced, potentially to zero. A
//! permissionless function should be provided in order to allow bookkeeping to be updated in this
//! instance. E.g. some balance is frozen when it is used for voting, one could use held balance for
//! voting, but nothing prevents this frozen balance from being reduced if the overlapping hold is
//! slashed.
//!
//! Every Hold and Freeze is accompanied by a unique `Reason`, making it clear for each instance
//! what the originating pallet and purpose is. These reasons are amalgomated into a single enum
//! `RuntimeHoldReason` and `RuntimeFreezeReason` respectively, when the runtime is compiled.
//!
//! Note that `Hold` and `Freeze` reasons should remain in your runtime for as long as storage
//! could exist in your runtime with those reasons, otherwise your runtime state could become
//! undecodable.
//!
//! ### Should I use a Hold or Freeze?
//!
//! If you require a balance to be infaillibly slashed, then you should use Holds.
//!
//! If you require setting a minimum account balance amount, then you should use a Freezes. Note
//! Freezes do not carry the same guarantees as Holds. Although the account cannot voluntarily
//! reduce their balance below the largest freeze, if Holds on the account are slashed then the
//! balance could drop below the freeze amount.
//!
//! ## Sets of Tokens
//!
//! For managing sets of tokens, see the [`fungibles`](`frame_support::traits::fungibles`) trait
//! which is a wrapper around this trait but supporting multiple asset instances.
//!
//! [`frame_tokens`]: ../../../../polkadot_sdk_docs/reference_docs/frame_tokens/index.html

pub mod conformance_tests;
pub mod freeze;
pub mod hold;
pub(crate) mod imbalance;
mod item_of;
mod lifetime;
pub mod metadata;
mod regular;
mod union_of;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::marker::PhantomData;
use frame_support_procedural::{CloneNoBound, DebugNoBound, EqNoBound, PartialEqNoBound};
use scale_info::TypeInfo;
#[cfg(feature = "runtime-benchmarks")]
use sp_runtime::Saturating;

use super::{
	Fortitude::{Force, Polite},
	Precision::BestEffort,
};
pub use freeze::{Inspect as InspectFreeze, Mutate as MutateFreeze};
pub use hold::{
	Balanced as BalancedHold, Inspect as InspectHold, Mutate as MutateHold,
	Unbalanced as UnbalancedHold,
};
pub use imbalance::{Credit, Debt, HandleImbalanceDrop, Imbalance};
pub use item_of::ItemOf;
pub use lifetime::Create;
pub use regular::{
	Balanced, DecreaseIssuance, Dust, IncreaseIssuance, Inspect, Mutate, Unbalanced,
};
use sp_arithmetic::traits::Zero;
use sp_core::Get;
use sp_runtime::{traits::Convert, DispatchError};
pub use union_of::{NativeFromLeft, NativeOrWithId, UnionOf};

use crate::{
	ensure,
	traits::{Consideration, Footprint, MaybeConsideration},
};

/// Consideration method using a `fungible` balance frozen as the cost exacted for the footprint.
///
/// The aggregate amount frozen under `R::get()` for any account which has multiple tickets,
/// is the *cumulative* amounts of each ticket's footprint (each individually determined by `D`).
#[derive(
	CloneNoBound, EqNoBound, PartialEqNoBound, Encode, Decode, TypeInfo, MaxEncodedLen, DebugNoBound,
)]
#[scale_info(skip_type_params(A, F, R, D, Fp))]
#[codec(mel_bound())]
pub struct FreezeConsideration<A, F, R, D, Fp>(F::Balance, PhantomData<fn() -> (A, R, D, Fp)>)
where
	F: MutateFreeze<A>;
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateFreeze<A>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateFreeze<A> + Mutate<A>,
		R: 'static + Get<F::Id>,
		D: 'static + Convert<Fp, F::Balance>,
		Fp: 'static,
	> Consideration<A, Fp> for FreezeConsideration<A, F, R, D, Fp>
{
	fn new(who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		let new = D::convert(footprint);
		F::increase_frozen(&R::get(), who, new)?;
		Ok(Self(new, PhantomData))
	}
	fn update(self, who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		let new = D::convert(footprint);
		if self.0 > new {
			F::decrease_frozen(&R::get(), who, self.0 - new)?;
		} else if new > self.0 {
			F::increase_frozen(&R::get(), who, new - self.0)?;
		}
		Ok(Self(new, PhantomData))
	}
	fn drop(self, who: &A) -> Result<(), DispatchError> {
		F::decrease_frozen(&R::get(), who, self.0).map(|_| ())
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(who: &A, fp: Fp) {
		let _ = F::mint_into(who, F::minimum_balance().saturating_add(D::convert(fp)));
	}
}
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateFreeze<A>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateFreeze<A> + Mutate<A>,
		R: 'static + Get<F::Id>,
		D: 'static + Convert<Fp, F::Balance>,
		Fp: 'static,
	> MaybeConsideration<A, Fp> for FreezeConsideration<A, F, R, D, Fp>
{
	fn is_none(&self) -> bool {
		self.0.is_zero()
	}
}

/// Consideration method using a `fungible` balance frozen as the cost exacted for the footprint.
#[derive(
	CloneNoBound,
	EqNoBound,
	PartialEqNoBound,
	Encode,
	Decode,
	DecodeWithMemTracking,
	TypeInfo,
	MaxEncodedLen,
	DebugNoBound,
)]
#[scale_info(skip_type_params(A, F, R, D, Fp))]
#[codec(mel_bound())]
pub struct HoldConsideration<A, F, R, D, Fp = Footprint>(
	F::Balance,
	PhantomData<fn() -> (A, R, D, Fp)>,
)
where
	F: MutateHold<A>;
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateHold<A>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateHold<A> + Mutate<A>,
		R: 'static + Get<F::Reason>,
		D: 'static + Convert<Fp, F::Balance>,
		Fp: 'static,
	> Consideration<A, Fp> for HoldConsideration<A, F, R, D, Fp>
{
	fn new(who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		let new = D::convert(footprint);
		F::hold(&R::get(), who, new)?;
		Ok(Self(new, PhantomData))
	}
	fn update(self, who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		let new = D::convert(footprint);
		if self.0 > new {
			F::release(&R::get(), who, self.0 - new, BestEffort)?;
		} else if new > self.0 {
			F::hold(&R::get(), who, new - self.0)?;
		}
		Ok(Self(new, PhantomData))
	}
	fn drop(self, who: &A) -> Result<(), DispatchError> {
		F::release(&R::get(), who, self.0, BestEffort).map(|_| ())
	}
	fn burn(self, who: &A) {
		let _ = F::burn_held(&R::get(), who, self.0, BestEffort, Force);
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(who: &A, fp: Fp) {
		let _ = F::mint_into(who, F::minimum_balance().saturating_add(D::convert(fp)));
	}
}
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateHold<A>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateHold<A> + Mutate<A>,
		R: 'static + Get<F::Reason>,
		D: 'static + Convert<Fp, F::Balance>,
		Fp: 'static,
	> MaybeConsideration<A, Fp> for HoldConsideration<A, F, R, D, Fp>
{
	fn is_none(&self) -> bool {
		self.0.is_zero()
	}
}

/// Basic consideration method using a `fungible` balance frozen as the cost exacted for the
/// footprint.
///
/// NOTE: This is an optimized implementation, which can only be used for systems where each
/// account has only a single active ticket associated with it since individual tickets do not
/// track the specific balance which is frozen. If you are uncertain then use `FreezeConsideration`
/// instead, since this works in all circumstances.
#[derive(
	CloneNoBound, EqNoBound, PartialEqNoBound, Encode, Decode, TypeInfo, MaxEncodedLen, DebugNoBound,
)]
#[scale_info(skip_type_params(A, Fx, Rx, D, Fp))]
#[codec(mel_bound())]
pub struct LoneFreezeConsideration<A, Fx, Rx, D, Fp>(PhantomData<fn() -> (A, Fx, Rx, D, Fp)>);
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] Fx: 'static + MutateFreeze<A>,
		#[cfg(feature = "runtime-benchmarks")] Fx: 'static + MutateFreeze<A> + Mutate<A>,
		Rx: 'static + Get<Fx::Id>,
		D: 'static + Convert<Fp, Fx::Balance>,
		Fp: 'static,
	> Consideration<A, Fp> for LoneFreezeConsideration<A, Fx, Rx, D, Fp>
{
	fn new(who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		ensure!(Fx::balance_frozen(&Rx::get(), who).is_zero(), DispatchError::Unavailable);
		Fx::set_frozen(&Rx::get(), who, D::convert(footprint), Polite).map(|_| Self(PhantomData))
	}
	fn update(self, who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		Fx::set_frozen(&Rx::get(), who, D::convert(footprint), Polite).map(|_| Self(PhantomData))
	}
	fn drop(self, who: &A) -> Result<(), DispatchError> {
		Fx::thaw(&Rx::get(), who).map(|_| ())
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(who: &A, fp: Fp) {
		let _ = Fx::mint_into(who, Fx::minimum_balance().saturating_add(D::convert(fp)));
	}
}

/// Basic consideration method using a `fungible` balance placed on hold as the cost exacted for the
/// footprint.
///
/// NOTE: This is an optimized implementation, which can only be used for systems where each
/// account has only a single active ticket associated with it since individual tickets do not
/// track the specific balance which is frozen. If you are uncertain then use `FreezeConsideration`
/// instead, since this works in all circumstances.
#[derive(
	CloneNoBound, EqNoBound, PartialEqNoBound, Encode, Decode, TypeInfo, MaxEncodedLen, DebugNoBound,
)]
#[scale_info(skip_type_params(A, Fx, Rx, D, Fp))]
#[codec(mel_bound())]
pub struct LoneHoldConsideration<A, Fx, Rx, D, Fp>(PhantomData<fn() -> (A, Fx, Rx, D, Fp)>);
impl<
		A: 'static + Eq,
		#[cfg(not(feature = "runtime-benchmarks"))] F: 'static + MutateHold<A>,
		#[cfg(feature = "runtime-benchmarks")] F: 'static + MutateHold<A> + Mutate<A>,
		R: 'static + Get<F::Reason>,
		D: 'static + Convert<Fp, F::Balance>,
		Fp: 'static,
	> Consideration<A, Fp> for LoneHoldConsideration<A, F, R, D, Fp>
{
	fn new(who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		ensure!(F::balance_on_hold(&R::get(), who).is_zero(), DispatchError::Unavailable);
		F::set_on_hold(&R::get(), who, D::convert(footprint)).map(|_| Self(PhantomData))
	}
	fn update(self, who: &A, footprint: Fp) -> Result<Self, DispatchError> {
		F::set_on_hold(&R::get(), who, D::convert(footprint)).map(|_| Self(PhantomData))
	}
	fn drop(self, who: &A) -> Result<(), DispatchError> {
		F::release_all(&R::get(), who, BestEffort).map(|_| ())
	}
	fn burn(self, who: &A) {
		let _ = F::burn_all_held(&R::get(), who, BestEffort, Force);
	}
	#[cfg(feature = "runtime-benchmarks")]
	fn ensure_successful(who: &A, fp: Fp) {
		let _ = F::mint_into(who, F::minimum_balance().saturating_add(D::convert(fp)));
	}
}
