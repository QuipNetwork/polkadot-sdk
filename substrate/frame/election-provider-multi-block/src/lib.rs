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

//! # Multi-phase, multi-block, election provider pallet.
//!
//! > This pallet is sometimes abbreviated as `EPMB`, and `pallet_election_provider_multi_phase` as
//! > `EPM`.
//!
//! ## Overall idea
//!
//! `pallet_election_provider_multi_phase` provides the basic ability for NPoS solutions to be
//! computed offchain (essentially anywhere) and submitted back to the chain as signed or unsigned
//! transaction, with sensible configurations and fail-safe mechanisms to ensure system safety.
//! Nonetheless, it has a limited capacity in terms of number of voters it can process in a **single
//! block**.
//!
//! This pallet takes `EPM` system, keeps most of its ideas and core premises, and extends it to
//! support paginated, multi-block operations. The final goal of this pallet is to scale linearly
//! with the number of blocks allocated to the elections. Moreover, the amount of work that it does
//! in one block should be bounded and measurable, making it suitable for a parachain. In principle,
//! with large enough blocks (in a dedicated parachain), the number of voters included in the NPoS
//! system can grow significantly (yet, obviously not indefinitely).
//!
//! Note that this pallet does not consider how the recipient is processing the results. To ensure
//! scalability, the recipient of this pallet's data (i.e. `pallet-staking`) must also be capable of
//! pagination and multi-block processing.
//!
//! ## Companion pallets
//!
//! This pallet will only function in a sensible way if it is peered with its companion pallets.
//!
//! - The [`verifier`] pallet provides a standard implementation of the [`verifier::Verifier`].
//! - The [`unsigned`] module provides the implementation of unsigned submission by validators. If
//!   this pallet is included, then [`Config::UnsignedPhase`] will determine its duration.
//! - The [`signed`] module provides the implementation of the signed submission by any account. If
//!   this pallet is included, the combined [`Config::SignedPhase`] and
//!   [`Config::SignedValidationPhase`] will determine its duration
//!
//! These pallets are in fact hierarchical. This particular one is the top level one. It contains
//! the shared information that all child pallets use. All child pallets depend on the top level
//! pallet ONLY, but not the other way around. For those cases, traits are used.
//!
//! For reverse linking, or child-linking, only explicit traits with clear interfaces are used. For
//! example, the following traits facilitate other communication:
//!
//! * [`crate::types::SignedInterface`]: Parent talking to signed.
//! * [`crate::verifier::Verifier`]: Parent talking to verifier.
//! * [`crate::verifier::SolutionDataProvider`]: Verifier talking to signed.

//! ## Pagination
//!
//! Most of the external APIs of this pallet are paginated. All pagination follow a pattern where if
//! `N` pages exist, the first paginated call is `function(N-1)` and the last one is `function(0)`.
//! For example, with 3 pages, the `elect` of [`ElectionProvider`] is expected to be called as
//! `elect(2) -> elect(1) -> elect(0)`. In essence, calling a paginated function with index 0 is
//! always a signal of termination, meaning that no further calls will follow.
//!
//! The snapshot creation for voters (Nominators in staking), submission of signed pages, validation
//! of signed solutions and exporting of pages are all paginated. Note that this pallet is yet to
//! support paginated target (Validators in staking) snapshotting.
//!
//! ### Terminology Note: `msp` and `lsp`
//!
//! Stand for _most significant page_ (n-1) and _least significant page_ (0).
//!
//! See [`ElectionProvider::msp`] and [`ElectionProvider::lsp`], and their usage.
//!
//! ## Phases
//!
//! The operations in this pallet are divided into rounds, a `u32` number stored in [`Round`].
//! This value helps this pallet organize itself, and leaves the door open for lazy deletion of any
//! stale data. A round, under the happy path, starts by receiving the call to
//! [`ElectionProvider::start`], and is terminated by receiving a call to
//! [`ElectionProvider::elect`] with value 0.
//!
//! The timeline of pallet is overall as follows:
//!
//! ```ignore
//!  <  Off  >
//! 0 ------------ 12 13 14 15 ----------- 20 ---------25 ------- 30
//! 	           |       |              |            |          |
//! 	     Snapshot      Signed   SignedValidation  Unsigned   Elect
//! ```
//!
//! * Duration of `Snapshot` is determined by [`Config::Pages`] + 1.
//! 	* Whereby in the first page we take the "Targets" snapshot, and in the subsequent pages we take
//!    the voter snapshot.
//! 	* For example, with `Pages = 4`:
//! 		* `Snapshot(4)` -> `Targets(all)`
//! 		* `Snapshot(3)` -> `Voters(3)`
//! 		* `Snapshot(2)` -> `Voters(2)`
//! 		* `Snapshot(1)` -> `Voters(1)`
//! 		* `Snapshot(0)` -> `Voters(0)`
//! * Duration of `Signed`, `SignedValidation` and `Unsigned` are determined by
//!   [`Config::SignedPhase`], [`Config::SignedValidationPhase`] and [`Config::UnsignedPhase`]
//!   respectively.
//! * [`Config::Pages`] calls to elect are expected, but all in all the pallet will close a round
//!   once `elect(0)` is called.
//!
//! > Given this, it is rather important for the user of this pallet to ensure it always terminates
//! > election via `elect` before requesting a new one.
//!
//! ### Phase Transition
//!
//! Within all 4 pallets only the parent pallet is allowed to move the phases forward. As of now,
//! the transition happens `on-poll`, ensuring that we don't consume too much weight. The parent
//! pallet is in charge of aggregating the work to be done by all pallets, checking if it can fit
//! within the current block's weight limits, and executing it if so.
//!
//! Occasional phase transition stalling is not a critical issue. Every instance of phase transition
//! failing is accompanied by a [`Event::UnexpectedPhaseTransitionOutOfWeight`] for visibility.
//!
//! Note this pallet transitions phases all the way into [`crate::types::Phase::Done`]. At this
//! point, we will move to `Export` phase an onwards by calls into `elect`. A call to `elect(0)`
//! rotates the round, as stated above.
//!
//! ## Feasible Solution (correct solution)
//!
//! All submissions must undergo a feasibility check. Signed solutions are checked one by one at the
//! end of the signed phase, and the unsigned solutions are checked on the spot. A feasible solution
//! is as follows:
//!
//! 0. **all** of the used indices must be correct.
//! 1. present *exactly* correct number of winners.
//! 2. any assignment is checked to match with `PagedVoterSnapshot`.
//! 3. the claimed score is valid, based on the fixed point arithmetic accuracy.
//!
//! More about this in [`verifier`], who is responsible for doing all of the above.
//!
//! ### Fallback and Emergency
//!
//! If at any page, [`ElectionProvider::elect`] fails, a call with the same page-index is dispatched
//! to [`Config::Fallback`]. [`Config::Fallback`] is itself (yet) another implementation of
//! [`ElectionProvider`], and can decide to do anything, but a few reasonable options are provided
//! here:
//!
//! 1. Do nothing: [`Continue`]
//! 2. Force us into the emergency phase: [`crate::InitiateEmergencyPhase`]. This initiates
//!    [`Phase::Emergency`], which will halt almost all operations of this pallet, and it can only
//!    be recovered by [`AdminOperation`], dispatched via [`Call::manage`].
//! 3. compute an onchain from the give page of snapshot.
//!
//! Note that configuring the fallback to be onchain computation is not recommended, unless for
//! test-nets for a number of reasons:
//!
//! 1. The solution score of fallback is never checked to match the "minimum" score. That being
//!    said, the computation happens onchain so we can trust it.
//! 2. The onchain fallback runs on the same number of voters and targets that reside on a single
//!    page of a snapshot, which will very likely be too much for actual onchain computation. Yet,
//!    we don't have another choice as we cannot request another smaller snapshot from the data
//!    provider mid-election without more bookkeeping on the staking side.
//!
//! If onchain solution is to be seriously considered, an improvement to this pallet should
//! re-request a smaller set of voters from `T::DataProvider` in a stateless manner.
//!
//! ### Signed Phase
//!
//! Signed phase is when an offchain miner, aka, `polkadot-staking-miner` should operate upon. See
//! [`signed`] for more information.
//!
//! ## Unsigned Phase
//!
//! Unsigned phase is a built-in fallback in which validators may submit a single page election,
//! taking into account only the [`ElectionProvider::msp`] (_most significant page_). See
//! [`crate::unsigned`] for more information.

// Implementation notes:
//
// - Naming convention is: `${singular}_page` for singular, e.g. `voter_page` for `Vec<Voter>`.
//   `paged_${plural}` for plural, e.g. `paged_voters` for `Vec<Vec<Voter>>`.
//
// - Since this crate has multiple `Pallet` and `Configs`, in each sub-pallet, we only reference the
//   local `Pallet` without a prefix and allow it to be imported via `use`. Avoid `super::Pallet`
//   except for the case of a modules that want to reference their local `Pallet` . The
//   `crate::Pallet` is always reserved for the parent pallet. Other sibling pallets must be
//   referenced with full path, e.g. `crate::Verifier::Pallet`. Do NOT write something like `use
//   unsigned::Pallet as UnsignedPallet`.
//
// - Respecting private storage items with wrapper We move all implementations out of the `mod
//   pallet` as much as possible to ensure we NEVER access the internal storage items directly. All
//   operations should happen with the wrapper types.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(any(feature = "runtime-benchmarks", test))]
use crate::signed::{CalculateBaseDeposit, CalculatePageDeposit};
use crate::verifier::{AsynchronousVerifier, Verifier};
use codec::{Decode, Encode, MaxEncodedLen};
use frame_election_provider_support::{
	onchain, BoundedSupportsOf, DataProviderBounds, ElectionDataProvider, ElectionProvider,
	InstantElectionProvider,
};
use frame_support::{
	dispatch::PostDispatchInfo,
	pallet_prelude::*,
	traits::{Defensive, EnsureOrigin},
	weights::WeightMeter,
	DebugNoBound, Twox64Concat,
};
use frame_system::pallet_prelude::*;
use scale_info::TypeInfo;
use sp_arithmetic::{
	traits::{CheckedAdd, Zero},
	PerThing, UpperOf,
};
use sp_npos_elections::{EvaluateSupport, VoteWeight};
use sp_runtime::{
	traits::{Hash, Saturating},
	SaturatedConversion,
};
use sp_std::{borrow::ToOwned, boxed::Box, prelude::*};

#[cfg(test)]
mod mock;
#[macro_use]
pub mod helpers;
#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;

/// The common logging prefix of all pallets in this crate.
pub const LOG_PREFIX: &'static str = "runtime::multiblock-election";

macro_rules! clear_round_based_map {
	($map: ty, $round: expr) => {{
		let __r = <$map>::clear_prefix($round, u32::MAX, None);
		debug_assert!(__r.unique <= T::Pages::get(), "clearing map caused too many removals")
	}};
}

/// The signed pallet
pub mod signed;
/// Common types of the pallet
pub mod types;
/// The unsigned pallet
pub mod unsigned;
/// The verifier pallet
pub mod verifier;
/// The weight module
pub mod weights;

pub use pallet::*;
pub use types::*;
pub use weights::traits::pallet_election_provider_multi_block::WeightInfo;

/// A fallback implementation that transitions the pallet to the emergency phase.
pub struct InitiateEmergencyPhase<T>(sp_std::marker::PhantomData<T>);
impl<T: Config> ElectionProvider for InitiateEmergencyPhase<T> {
	type AccountId = T::AccountId;
	type BlockNumber = BlockNumberFor<T>;
	type DataProvider = T::DataProvider;
	type Error = &'static str;
	type Pages = T::Pages;
	type MaxBackersPerWinner = <T::Verifier as Verifier>::MaxBackersPerWinner;
	type MaxWinnersPerPage = <T::Verifier as Verifier>::MaxWinnersPerPage;
	type MaxBackersPerWinnerFinal = <T::Verifier as Verifier>::MaxBackersPerWinnerFinal;

	fn elect(_page: PageIndex) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		Pallet::<T>::phase_transition(Phase::Emergency);
		Err("Emergency phase started.")
	}

	fn status() -> Result<Option<Weight>, ()> {
		Ok(Some(Default::default()))
	}

	fn start() -> Result<(), Self::Error> {
		Ok(())
	}

	fn duration() -> Self::BlockNumber {
		Zero::zero()
	}
}

impl<T: Config> InstantElectionProvider for InitiateEmergencyPhase<T> {
	fn instant_elect(
		_voters: Vec<VoterOf<T::MinerConfig>>,
		_targets: Vec<Self::AccountId>,
		_desired_targets: u32,
	) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		Self::elect(0)
	}

	fn bother() -> bool {
		false
	}
}

/// A fallback implementation that silently continues into the next page.
///
/// This is suitable for onchain usage.
pub struct Continue<T>(sp_std::marker::PhantomData<T>);
impl<T: Config> ElectionProvider for Continue<T> {
	type AccountId = T::AccountId;
	type BlockNumber = BlockNumberFor<T>;
	type DataProvider = T::DataProvider;
	type Error = &'static str;
	type Pages = T::Pages;
	type MaxBackersPerWinner = <T::Verifier as Verifier>::MaxBackersPerWinner;
	type MaxWinnersPerPage = <T::Verifier as Verifier>::MaxWinnersPerPage;
	type MaxBackersPerWinnerFinal = <T::Verifier as Verifier>::MaxBackersPerWinnerFinal;

	fn elect(_page: PageIndex) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		Err("'Continue' fallback will do nothing")
	}

	fn start() -> Result<(), Self::Error> {
		Ok(())
	}

	fn duration() -> Self::BlockNumber {
		Zero::zero()
	}

	fn status() -> Result<Option<Weight>, ()> {
		Ok(Some(Default::default()))
	}
}

impl<T: Config> InstantElectionProvider for Continue<T> {
	fn instant_elect(
		_voters: Vec<VoterOf<T::MinerConfig>>,
		_targets: Vec<Self::AccountId>,
		_desired_targets: u32,
	) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		Self::elect(0)
	}

	fn bother() -> bool {
		false
	}
}

/// A easy means to configure [`Config::AreWeDone`].
///
/// With this, you can say what to do if a solution is queued, or what to do if not.
///
/// Two common shorthands of this are provided:
/// * [`ProceedRegardlessOf`]
/// * [`RevertToSignedIfNotQueuedOf`]
pub struct IfSolutionQueuedElse<T, Queued, NotQueued>(
	sp_std::marker::PhantomData<(T, Queued, NotQueued)>,
);

/// A `Get` impl for `Phase::Done`
pub struct GetDone<T>(sp_std::marker::PhantomData<T>);
impl<T: Config> Get<Phase<T>> for GetDone<T> {
	fn get() -> Phase<T> {
		Phase::Done
	}
}

/// A `Get` impl for `Phase::Signed(T::SignedPhase::get())`
pub struct GetSigned<T>(sp_std::marker::PhantomData<T>);
impl<T: Config> Get<Phase<T>> for GetSigned<T> {
	fn get() -> Phase<T> {
		Phase::Signed(T::SignedPhase::get().saturating_sub(1u32.into()))
	}
}

/// A shorthand for [`IfSolutionQueuedElse`] that proceeds regardless of the solution being queued.
pub type ProceedRegardlessOf<T> = IfSolutionQueuedElse<T, GetDone<T>, GetDone<T>>;

/// A shorthand for [`IfSolutionQueuedElse`] that proceeds to `Phase::Done` if the solution is
/// queued. Otherwise, it proceeds to `Phase::Signed`.
pub type RevertToSignedIfNotQueuedOf<T> = IfSolutionQueuedElse<T, GetDone<T>, GetSigned<T>>;

impl<T: Config, Queued, NotQueued> IfSolutionQueuedElse<T, Queued, NotQueued> {
	fn something_queued() -> bool {
		let queued_score = <T::Verifier as verifier::Verifier>::queued_score().is_some();
		#[cfg(debug_assertions)]
		{
			let any_pages_queued = (Pallet::<T>::lsp()..=Pallet::<T>::msp()).any(|p| {
				<T::Verifier as verifier::Verifier>::get_queued_solution_page(p).is_some()
			});
			assert_eq!(
				queued_score, any_pages_queued,
				"queued score ({}) and queued pages ({}) must match",
				queued_score, any_pages_queued
			);
		}
		queued_score
	}
}

impl<T: Config, Queued: Get<Phase<T>>, NotQueued: Get<Phase<T>>> Get<Phase<T>>
	for IfSolutionQueuedElse<T, Queued, NotQueued>
{
	fn get() -> Phase<T> {
		if Self::something_queued() {
			Queued::get()
		} else {
			NotQueued::get()
		}
	}
}

/// Internal errors of the pallet. This is used in the implementation of [`ElectionProvider`].
///
/// Note that this is different from [`pallet::Error`].
#[derive(
	frame_support::DebugNoBound, frame_support::PartialEqNoBound, frame_support::EqNoBound,
)]
pub enum ElectionError<T: Config> {
	/// An error happened in the feasibility check sub-system.
	Feasibility(verifier::FeasibilityError),
	/// An error in the fallback.
	Fallback(FallbackErrorOf<T>),
	/// An error in the onchain seq-phragmen implementation
	OnChain(onchain::Error),
	/// An error happened in the data provider.
	DataProvider(&'static str),
	/// the corresponding page in the queued supports is not available.
	SupportPageNotAvailable,
	/// The election is not ongoing and therefore no results may be queried.
	NotOngoing,
	/// The election is currently ongoing, and therefore we cannot start again.
	Ongoing,
	/// Called elect() with wrong page order or in wrong phase.
	OutOfOrder,
	/// Other misc error
	Other(&'static str),
}

impl<T: Config> From<onchain::Error> for ElectionError<T> {
	fn from(e: onchain::Error) -> Self {
		ElectionError::OnChain(e)
	}
}

impl<T: Config> From<verifier::FeasibilityError> for ElectionError<T> {
	fn from(e: verifier::FeasibilityError) -> Self {
		ElectionError::Feasibility(e)
	}
}

/// Different operations that only the [`Config::AdminOrigin`] can perform on the pallet.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	DebugNoBound,
	CloneNoBound,
	PartialEqNoBound,
	EqNoBound,
)]
#[codec(mel_bound(T: Config))]
#[scale_info(skip_type_params(T))]
pub enum AdminOperation<T: Config> {
	/// Set the given (single page) emergency solution.
	///
	/// Can only be called in emergency phase.
	EmergencySetSolution(Box<BoundedSupportsOf<Pallet<T>>>, ElectionScore),
	/// Set the minimum untrusted score. This is directly communicated to the verifier component to
	/// be taken into account.
	///
	/// This is useful in preventing any serious issue where due to a bug we accept a very bad
	/// solution.
	SetMinUntrustedScore(ElectionScore),
}

/// Different operations that the [`Config::ManagerOrigin`] (or [`Config::AdminOrigin`]) can perform
/// on the pallet.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	MaxEncodedLen,
	TypeInfo,
	DebugNoBound,
	CloneNoBound,
	PartialEqNoBound,
	EqNoBound,
)]
#[codec(mel_bound(T: Config))]
#[scale_info(skip_type_params(T))]
pub enum ManagerOperation<T: Config> {
	/// Forcefully go to the next round, starting from the Off Phase.
	ForceRotateRound,
	/// Force-set the phase to the given phase.
	///
	/// This can have many many combinations, use only with care and sufficient testing.
	ForceSetPhase(Phase<T>),
	/// Trigger the (single page) fallback in `instant` mode, with the given parameters, and
	/// queue it if correct.
	///
	/// Can only be called in emergency phase.
	EmergencyFallback,
}

/// Trait to notify other sub-systems that a round has ended.
pub trait OnRoundRotation {
	/// `ending` round has ended. Implies we are now at round `ending + 1`
	fn on_round_rotation(ending: u32);
}

impl OnRoundRotation for () {
	fn on_round_rotation(_: u32) {}
}

/// An implementation of [`OnRoundRotation`] that immediately deletes all the data in all the
/// pallets, once the round is over.
///
/// This is intended to be phased out once we move to fully lazy deletion system to spare more PoV.
/// In that case, simply use `()` on [`pallet::Config::OnRoundRotation`].
pub struct CleanRound<T>(core::marker::PhantomData<T>);
impl<T: Config> OnRoundRotation for CleanRound<T> {
	fn on_round_rotation(_ending: u32) {
		// Kill everything in the verifier.
		T::Verifier::kill();

		// Kill the snapshot.
		pallet::Snapshot::<T>::kill();

		// Nothing to do in the signed pallet -- it is already in lazy-deletion mode.
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Duration of the unsigned phase.
		#[pallet::constant]
		type UnsignedPhase: Get<BlockNumberFor<Self>>;
		/// Duration of the signed phase.
		#[pallet::constant]
		type SignedPhase: Get<BlockNumberFor<Self>>;
		/// Duration of the singed validation phase.
		///
		/// The duration of this should not be less than `T::Pages`, and there is no point in it
		/// being more than `SignedPhase::MaxSubmission::get() * T::Pages`.
		#[pallet::constant]
		type SignedValidationPhase: Get<BlockNumberFor<Self>>;

		/// The number of snapshot voters to fetch per block.
		#[pallet::constant]
		type VoterSnapshotPerBlock: Get<u32>;

		/// The number of snapshot targets to fetch per block.
		#[pallet::constant]
		type TargetSnapshotPerBlock: Get<u32>;

		/// The number of pages.
		///
		/// The snapshot is created with this many keys in the storage map.
		///
		/// The solutions may contain at MOST this many pages, but less pages are acceptable as
		/// well.
		#[pallet::constant]
		type Pages: Get<PageIndex>;

		/// Something that will provide the election data.
		type DataProvider: ElectionDataProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
		>;

		/// The miner configuration.
		///
		/// These configurations are passed to [`crate::unsigned::miner::BaseMiner`]. An external
		/// miner implementation should implement this trait, and use the said `BaseMiner`.
		type MinerConfig: crate::unsigned::miner::MinerConfig<
			Pages = Self::Pages,
			AccountId = <Self as frame_system::Config>::AccountId,
			MaxVotesPerVoter = <Self::DataProvider as ElectionDataProvider>::MaxVotesPerVoter,
			VoterSnapshotPerBlock = Self::VoterSnapshotPerBlock,
			TargetSnapshotPerBlock = Self::TargetSnapshotPerBlock,
			MaxBackersPerWinner = <Self::Verifier as verifier::Verifier>::MaxBackersPerWinner,
			MaxWinnersPerPage = <Self::Verifier as verifier::Verifier>::MaxWinnersPerPage,
		>;

		/// The fallback type used for the election.
		type Fallback: InstantElectionProvider<
			AccountId = Self::AccountId,
			BlockNumber = BlockNumberFor<Self>,
			DataProvider = Self::DataProvider,
			MaxBackersPerWinner = <Self::Verifier as verifier::Verifier>::MaxBackersPerWinner,
			MaxWinnersPerPage = <Self::Verifier as verifier::Verifier>::MaxWinnersPerPage,
		>;

		/// The verifier pallet's interface.
		type Verifier: verifier::Verifier<
				Solution = SolutionOf<Self::MinerConfig>,
				AccountId = Self::AccountId,
			> + verifier::AsynchronousVerifier;

		/// Interface signed pallet's interface.
		type Signed: SignedInterface;

		/// The origin that can perform administration operations on this pallet.
		///
		/// This is the highest privilege origin of this pallet, and should be configured
		/// restrictively.
		type AdminOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// A less privileged origin than [`Config::AdminOrigin`], that can manage some election
		/// aspects, without critical power.
		type ManagerOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// An indicator of whether we should move to do the [`crate::types::Phase::Done`] or not?
		/// This is called at the end of the election process.
		///
		/// Common implementation is [`ProceedRegardlessOf`] or [`RevertToSignedIfNotQueuedOf`].
		type AreWeDone: Get<Phase<Self>>;

		/// The weight of the pallet.
		type WeightInfo: WeightInfo;

		/// Single type that implement [`super::OnRoundRotation`] to do something when the round
		/// ends.
		type OnRoundRotation: super::OnRoundRotation;
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Manage this pallet.
		///
		/// The origin of this call must be [`Config::ManagerOrigin`].
		///
		/// See [`ManagerOperation`] for various operations that are possible.
		#[pallet::weight(T::WeightInfo::manage_fallback().max(T::WeightInfo::export_terminal()))]
		#[pallet::call_index(0)]
		pub fn manage(origin: OriginFor<T>, op: ManagerOperation<T>) -> DispatchResultWithPostInfo {
			T::ManagerOrigin::ensure_origin(origin.clone()).map(|_| ()).or_else(|_| {
				// try admin origin as well as admin is a superset.
				T::AdminOrigin::ensure_origin(origin).map(|_| ())
			})?;
			match op {
				ManagerOperation::EmergencyFallback => {
					ensure!(Self::current_phase() == Phase::Emergency, Error::<T>::UnexpectedPhase);
					// note: for now we run this on the msp, but we can make it configurable if need
					// be.
					let voters = Snapshot::<T>::voters(Self::msp()).ok_or(Error::<T>::Snapshot)?;
					let targets = Snapshot::<T>::targets().ok_or(Error::<T>::Snapshot)?;
					let desired_targets =
						Snapshot::<T>::desired_targets().ok_or(Error::<T>::Snapshot)?;
					let fallback = T::Fallback::instant_elect(
						voters.into_inner(),
						targets.into_inner(),
						desired_targets,
					)
					.map_err(|e| {
						log!(warn, "Fallback failed: {:?}", e);
						Error::<T>::Fallback
					})?;
					let score = fallback.evaluate();
					T::Verifier::force_set_single_page_valid(fallback, 0, score);
					Ok(PostDispatchInfo {
						actual_weight: Some(T::WeightInfo::manage_fallback()),
						pays_fee: Pays::No,
					})
				},
				ManagerOperation::ForceSetPhase(phase) => {
					Self::phase_transition(phase);
					Ok(PostDispatchInfo {
						actual_weight: Some(T::DbWeight::get().reads_writes(1, 1)),
						pays_fee: Pays::No,
					})
				},
				ManagerOperation::ForceRotateRound => {
					Self::rotate_round();
					Ok(PostDispatchInfo {
						actual_weight: Some(T::WeightInfo::export_terminal()),
						pays_fee: Pays::No,
					})
				},
			}
		}

		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::admin_set())]
		pub fn admin(origin: OriginFor<T>, op: AdminOperation<T>) -> DispatchResultWithPostInfo {
			T::AdminOrigin::ensure_origin(origin)?;
			match op {
				AdminOperation::EmergencySetSolution(supports, score) => {
					ensure!(Self::current_phase() == Phase::Emergency, Error::<T>::UnexpectedPhase);
					T::Verifier::force_set_single_page_valid(*supports, 0, score);
					Ok(PostDispatchInfo {
						actual_weight: Some(T::WeightInfo::admin_set()),
						pays_fee: Pays::No,
					})
				},
				AdminOperation::SetMinUntrustedScore(score) => {
					T::Verifier::set_minimum_score(score);
					Ok(PostDispatchInfo {
						actual_weight: Some(T::DbWeight::get().reads_writes(1, 1)),
						pays_fee: Pays::No,
					})
				},
			}
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_poll(_now: BlockNumberFor<T>, weight_meter: &mut WeightMeter) {
			// first check we can at least read one storage.
			if !weight_meter.can_consume(T::DbWeight::get().reads(1)) {
				Self::deposit_event(Event::UnexpectedPhaseTransitionHalt {
					required: T::DbWeight::get().reads(1),
					had: weight_meter.remaining(),
				});
				return;
			}

			// if so, consume and prepare the next phase.
			let current_phase = Self::current_phase();
			weight_meter.consume(T::DbWeight::get().reads(1));

			let (self_weight, self_exec) = Self::per_block_exec(current_phase);
			let (verifier_weight, verifier_exc) = T::Verifier::per_block_exec();

			// The following will combine `Self::per_block_exec` and `T::Verifier::per_block_exec`
			// into a single tuple of `(Weight, Box<_>)`. Can be moved into a reusable combinator
			// function if we have this pattern in more places.
			let (combined_weight, combined_exec) = (
				// pre-exec weight is simply addition.
				self_weight.saturating_add(verifier_weight),
				// our new exec is..
				Box::new(move |meter: &mut WeightMeter| {
					self_exec(meter);
					verifier_exc(meter);
				}),
			);

			log!(
				trace,
				"worst-case required weight for transition from {:?} to {:?} is {:?}, has {:?}",
				current_phase,
				current_phase.next(),
				combined_weight,
				weight_meter.remaining()
			);
			if weight_meter.can_consume(combined_weight) {
				combined_exec(weight_meter);
			} else {
				Self::deposit_event(Event::UnexpectedPhaseTransitionOutOfWeight {
					from: current_phase,
					to: current_phase.next(),
					required: combined_weight,
					had: weight_meter.remaining(),
				});
			}

			// NOTE: why in here? because it is more accessible, for example `roll_to_with_ocw`.
			#[cfg(test)]
			{
				if _now > 200u32.into() {
					panic!("Looping to death: in case of errors in election start time in tests, we might loop \
					infinitely. This panic is preventing you from that. Double check `mock::ElectionStart` or increase \
					the 200 limit");
				}
				let test_election_start: BlockNumberFor<T> =
					(crate::mock::ElectionStart::get() as u32).into();
				if _now == test_election_start {
					crate::log!(info, "TESTING: Starting election at block {}", _now);
					crate::mock::MultiBlock::start().unwrap();
				}
			}
		}

		fn integrity_test() {
			use sp_std::mem::size_of;
			// The index type of both voters and targets need to be smaller than that of usize (very
			// unlikely to be the case, but anyhow).
			assert!(size_of::<SolutionVoterIndexOf<T::MinerConfig>>() <= size_of::<usize>());
			assert!(size_of::<SolutionTargetIndexOf<T::MinerConfig>>() <= size_of::<usize>());

			// also, because `VoterSnapshotPerBlock` and `TargetSnapshotPerBlock` are in u32, we
			// assert that both of these types are smaller than u32 as well.
			assert!(size_of::<SolutionVoterIndexOf<T::MinerConfig>>() <= size_of::<u32>());
			assert!(size_of::<SolutionTargetIndexOf<T::MinerConfig>>() <= size_of::<u32>());

			// pages must be at least 1.
			assert!(T::Pages::get() > 0);

			// Based on the requirements of [`sp_npos_elections::Assignment::try_normalize`].
			let max_vote: usize = <SolutionOf<T::MinerConfig> as NposSolution>::LIMIT;

			// 2. Maximum sum of [SolutionAccuracy; 16] must fit into `UpperOf<OffchainAccuracy>`.
			let maximum_chain_accuracy: Vec<UpperOf<SolutionAccuracyOf<T::MinerConfig>>> = (0..
				max_vote)
				.map(|_| {
					<UpperOf<SolutionAccuracyOf<T::MinerConfig>>>::from(
						<SolutionAccuracyOf<T::MinerConfig>>::one().deconstruct(),
					)
				})
				.collect();
			let _: UpperOf<SolutionAccuracyOf<T::MinerConfig>> = maximum_chain_accuracy
				.iter()
				.fold(Zero::zero(), |acc, x| acc.checked_add(x).unwrap());

			// We only accept data provider who's maximum votes per voter matches our
			// `T::Solution`'s `LIMIT`.
			//
			// NOTE that this pallet does not really need to enforce this in runtime. The
			// solution cannot represent any voters more than `LIMIT` anyhow.
			assert_eq!(
				<T::DataProvider as ElectionDataProvider>::MaxVotesPerVoter::get(),
				<SolutionOf<T::MinerConfig> as NposSolution>::LIMIT as u32,
			);

			// Either (signed + signed validation) is non-zero, or unsigned is non-zero
			let has_signed = !T::SignedPhase::get().is_zero();
			let signed_validation = T::SignedValidationPhase::get();
			let has_signed_validation = !signed_validation.is_zero();
			let has_unsigned = !T::UnsignedPhase::get().is_zero();
			assert!(
				has_signed == has_signed_validation,
				"Signed phase not set correct -- both should be set or unset"
			);
			assert!(
				signed_validation.is_zero() ||
					signed_validation % T::Pages::get().into() == Zero::zero(),
				"signed validation phase should be a multiple of the number of pages."
			);

			assert!(has_signed || has_unsigned, "either signed or unsigned phase must be set");
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(now: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state(now).map_err(Into::into)
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A phase transition happened. Only checks major changes in the variants, not minor inner
		/// values.
		PhaseTransitioned {
			/// the source phase
			from: Phase<T>,
			/// The target phase
			to: Phase<T>,
		},
		/// Target snapshot creation failed.
		UnexpectedTargetSnapshotFailed,
		/// Voter snapshot creation failed.
		UnexpectedVoterSnapshotFailed,
		/// Phase transition could not proceed due to being out of weight.
		UnexpectedPhaseTransitionOutOfWeight {
			from: Phase<T>,
			to: Phase<T>,
			required: Weight,
			had: Weight,
		},
		/// Phase transition could not even begin becaseu of being out of weight.
		UnexpectedPhaseTransitionHalt { required: Weight, had: Weight },
	}

	/// Error of the pallet that can be returned in response to dispatches.
	#[pallet::error]
	pub enum Error<T> {
		/// Triggering the `Fallback` failed.
		Fallback,
		/// Unexpected phase
		UnexpectedPhase,
		/// Snapshot was unavailable.
		Snapshot,
	}

	/// Common errors in all sub-pallets and miner.
	#[derive(PartialEq, Eq, Clone, Encode, Decode, Debug)]
	pub enum CommonError {
		/// Submission is too early (or too late, depending on your point of reference).
		EarlySubmission,
		/// The round counter is wrong.
		WrongRound,
		/// Submission is too weak to be considered an improvement.
		WeakSubmission,
		/// Wrong number of pages in the solution.
		WrongPageCount,
		/// Wrong number of winners presented.
		WrongWinnerCount,
		/// The snapshot fingerprint is not a match. The solution is likely outdated.
		WrongFingerprint,
		/// Snapshot was not available.
		Snapshot,
	}

	/// Internal counter for the number of rounds.
	///
	/// This is useful for de-duplication of transactions submitted to the pool, and general
	/// diagnostics of the pallet.
	///
	/// This is merely incremented once per every time that an upstream `elect` is called.
	#[pallet::storage]
	#[pallet::getter(fn round)]
	pub type Round<T: Config> = StorageValue<_, u32, ValueQuery>;

	/// Current phase.
	#[pallet::storage]
	#[pallet::getter(fn current_phase)]
	pub type CurrentPhase<T: Config> = StorageValue<_, Phase<T>, ValueQuery>;

	/// Wrapper struct for working with snapshots.
	///
	/// It manages the following storage items:
	///
	/// - `DesiredTargets`: The number of targets that we wish to collect.
	/// - `PagedVoterSnapshot`: Paginated map of voters.
	/// - `PagedVoterSnapshotHash`: Hash of the aforementioned.
	/// - `PagedTargetSnapshot`: Paginated map of targets.
	/// - `PagedTargetSnapshotHash`: Hash of the aforementioned.
	///
	/// ### Round
	///
	/// All inner storage items are keyed by the round number. Yet, none of the interface in this
	/// type expose this. This is because a snapshot is really only ever meaningful in the current
	/// round. Moreover, doing this will allow us to possibly lazy-delete the old round data, such
	/// as the sizeable snapshot, in a lazy manner. If any of these storage items, key-ed by a round
	/// index, are in a round that has passed, now they can be lazy deleted.
	///
	/// ### Invariants
	///
	/// The following invariants must be met at **all times** for this storage item to be "correct".
	///
	/// - `PagedVoterSnapshotHash` must always contain the correct the same number of keys, and the
	///   corresponding hash of the `PagedVoterSnapshot`.
	/// - `PagedTargetSnapshotHash` must always contain the correct the same number of keys, and the
	///   corresponding hash of the `PagedTargetSnapshot`.
	///
	/// - If any page from the paged voters/targets exists, then the aforementioned (desired
	///   targets) must also exist.
	///
	/// The following invariants might need to hold based on the current phase.
	///
	///   - If `Phase` IS `Snapshot(_)`, then partial voter/target pages must exist from `msp` to
	///     `lsp` based on the inner value.
	///   - If `Phase` IS `Off`, then, no snapshot must exist.
	///   - In all other phases, the snapshot must FULLY exist.
	pub(crate) struct Snapshot<T>(sp_std::marker::PhantomData<T>);
	impl<T: Config> Snapshot<T> {
		// ----------- mutable methods
		pub(crate) fn set_desired_targets(d: u32) {
			DesiredTargets::<T>::insert(Self::round(), d);
		}

		pub(crate) fn set_targets(targets: BoundedVec<T::AccountId, T::TargetSnapshotPerBlock>) {
			let hash = Self::write_storage_with_pre_allocate(
				&PagedTargetSnapshot::<T>::hashed_key_for(Self::round(), Pallet::<T>::msp()),
				targets,
			);
			PagedTargetSnapshotHash::<T>::insert(Self::round(), Pallet::<T>::msp(), hash);
		}

		pub(crate) fn set_voters(page: PageIndex, voters: VoterPageOf<T::MinerConfig>) {
			let hash = Self::write_storage_with_pre_allocate(
				&PagedVoterSnapshot::<T>::hashed_key_for(Self::round(), page),
				voters,
			);
			PagedVoterSnapshotHash::<T>::insert(Self::round(), page, hash);
		}

		/// Destroy the entire snapshot.
		///
		/// Should be called only once we transition to [`Phase::Off`].
		pub(crate) fn kill() {
			DesiredTargets::<T>::remove(Self::round());
			clear_round_based_map!(PagedVoterSnapshot::<T>, Self::round());
			clear_round_based_map!(PagedVoterSnapshotHash::<T>, Self::round());
			clear_round_based_map!(PagedTargetSnapshot::<T>, Self::round());
			clear_round_based_map!(PagedTargetSnapshotHash::<T>, Self::round());
		}

		// ----------- non-mutables
		pub(crate) fn desired_targets() -> Option<u32> {
			DesiredTargets::<T>::get(Self::round())
		}

		pub(crate) fn voters(page: PageIndex) -> Option<VoterPageOf<T::MinerConfig>> {
			PagedVoterSnapshot::<T>::get(Self::round(), page)
		}

		pub(crate) fn targets() -> Option<BoundedVec<T::AccountId, T::TargetSnapshotPerBlock>> {
			// NOTE: targets always have one index, which is 0, aka lsp.
			PagedTargetSnapshot::<T>::get(Self::round(), Pallet::<T>::msp())
		}

		/// Get a fingerprint of the snapshot, from all the hashes that are stored for each page of
		/// the snapshot.
		///
		/// This is computed as: `(target_hash, voter_hash_n, voter_hash_(n-1), ..., voter_hash_0)`
		/// where `n` is `T::Pages - 1`. In other words, it is the concatenated hash of targets, and
		/// voters, from `msp` to `lsp`.
		pub fn fingerprint() -> T::Hash {
			let mut hashed_target_and_voters =
				Self::targets_hash().unwrap_or_default().as_ref().to_vec();
			let hashed_voters = (Pallet::<T>::msp()..=Pallet::<T>::lsp())
				.map(|i| PagedVoterSnapshotHash::<T>::get(Self::round(), i).unwrap_or_default())
				.flat_map(|hash| <T::Hash as AsRef<[u8]>>::as_ref(&hash).to_owned())
				.collect::<Vec<u8>>();
			hashed_target_and_voters.extend(hashed_voters);
			T::Hashing::hash(&hashed_target_and_voters)
		}

		fn write_storage_with_pre_allocate<E: Encode>(key: &[u8], data: E) -> T::Hash {
			let size = data.encoded_size();
			let mut buffer = Vec::with_capacity(size);
			data.encode_to(&mut buffer);

			let hash = T::Hashing::hash(&buffer);

			// do some checks.
			debug_assert_eq!(buffer, data.encode());
			// buffer should have not re-allocated since.
			debug_assert!(buffer.len() == size && size == buffer.capacity());
			sp_io::storage::set(key, &buffer);

			hash
		}

		pub(crate) fn targets_hash() -> Option<T::Hash> {
			PagedTargetSnapshotHash::<T>::get(Self::round(), Pallet::<T>::msp())
		}

		fn round() -> u32 {
			Pallet::<T>::round()
		}
	}

	#[allow(unused)]
	#[cfg(any(test, feature = "runtime-benchmarks", feature = "try-runtime"))]
	impl<T: Config> Snapshot<T> {
		/// Ensure target snapshot exists.
		pub(crate) fn ensure_target_snapshot(exists: bool) -> Result<(), &'static str> {
			ensure!(exists ^ Self::desired_targets().is_none(), "desired target mismatch");
			ensure!(exists ^ Self::targets().is_none(), "targets mismatch");
			ensure!(exists ^ Self::targets_hash().is_none(), "targets hash mismatch");

			// and the hash is correct.
			if let Some(targets) = Self::targets() {
				let hash = Self::targets_hash().expect("must exist; qed");
				ensure!(hash == T::Hashing::hash(&targets.encode()), "targets hash mismatch");
			}
			Ok(())
		}

		/// Ensure voters exists, from page `T::Pages::get()` for `up_to_page` subsequent pages.
		pub(crate) fn ensure_voter_snapshot(
			exists: bool,
			mut up_to_page: PageIndex,
		) -> Result<(), &'static str> {
			up_to_page = up_to_page.min(T::Pages::get());
			// ensure that voter pages that should exist, indeed to exist..
			let mut sum_existing_voters: usize = 0;
			for p in (crate::Pallet::<T>::lsp()..=crate::Pallet::<T>::msp())
				.rev()
				.take(up_to_page as usize)
			{
				ensure!(
					(exists ^ Self::voters(p).is_none()) &&
						(exists ^ Self::voters_hash(p).is_none()),
					"voter page existence mismatch"
				);

				if let Some(voters_page) = Self::voters(p) {
					sum_existing_voters = sum_existing_voters.saturating_add(voters_page.len());
					let hash = Self::voters_hash(p).expect("must exist; qed");
					ensure!(hash == T::Hashing::hash(&voters_page.encode()), "voter hash mismatch");
				}
			}

			// ..and those that should not exist, indeed DON'T.
			for p in (crate::Pallet::<T>::lsp()..=crate::Pallet::<T>::msp())
				.take((T::Pages::get() - up_to_page) as usize)
			{
				ensure!(
					(exists ^ Self::voters(p).is_some()) &&
						(exists ^ Self::voters_hash(p).is_some()),
					"voter page non-existence mismatch"
				);
			}
			Ok(())
		}

		pub(crate) fn ensure_snapshot(
			exists: bool,
			mut up_to_page: PageIndex,
		) -> Result<(), &'static str> {
			Self::ensure_target_snapshot(exists)
				.and_then(|_| Self::ensure_voter_snapshot(exists, up_to_page))
		}

		pub(crate) fn ensure_full_snapshot() -> Result<(), &'static str> {
			// if any number of pages supposed to exist, these must also exist.
			ensure!(Self::desired_targets().is_some(), "desired target mismatch");
			ensure!(Self::targets_hash().is_some(), "targets hash mismatch");
			ensure!(
				Self::targets_decode_len().unwrap_or_default() as u32 ==
					T::TargetSnapshotPerBlock::get(),
				"targets decode length mismatch"
			);

			// ensure that voter pages that should exist, indeed to exist..
			for p in crate::Pallet::<T>::lsp()..=crate::Pallet::<T>::msp() {
				ensure!(
					Self::voters_hash(p).is_some() &&
						Self::voters_decode_len(p).unwrap_or_default() as u32 ==
							T::VoterSnapshotPerBlock::get(),
					"voter page existence mismatch"
				);
			}

			Ok(())
		}

		pub(crate) fn voters_decode_len(page: PageIndex) -> Option<usize> {
			PagedVoterSnapshot::<T>::decode_len(Self::round(), page)
		}

		pub(crate) fn targets_decode_len() -> Option<usize> {
			PagedTargetSnapshot::<T>::decode_len(Self::round(), Pallet::<T>::msp())
		}

		pub(crate) fn voters_hash(page: PageIndex) -> Option<T::Hash> {
			PagedVoterSnapshotHash::<T>::get(Self::round(), page)
		}

		pub(crate) fn sanity_check() -> Result<(), &'static str> {
			// check the snapshot existence based on the phase. This checks all of the needed
			// conditions except for the metadata values.
			let phase = Pallet::<T>::current_phase();
			let _ = match phase {
				// no page should exist in this phase.
				Phase::Off => Self::ensure_snapshot(false, T::Pages::get()),

				// we will star the snapshot in the next phase.
				Phase::Snapshot(p) if p == T::Pages::get() => {
					Self::ensure_snapshot(false, T::Pages::get())
				},
				// we are mid voter snapshot.
				Phase::Snapshot(p) if p < T::Pages::get() && p > 0 => {
					Self::ensure_snapshot(true, T::Pages::get() - p - 1)
				},
				// we cannot check anything in this block -- we take the last page of the snapshot.
				Phase::Snapshot(_) => Ok(()),

				// full snapshot must exist in these phases.
				Phase::Emergency |
				Phase::Signed(_) |
				Phase::SignedValidation(_) |
				Phase::Export(_) |
				Phase::Done |
				Phase::Unsigned(_) => Self::ensure_snapshot(true, T::Pages::get()),
			}?;

			Ok(())
		}
	}

	#[cfg(test)]
	impl<T: Config> Snapshot<T> {
		pub(crate) fn voter_pages() -> PageIndex {
			use sp_runtime::SaturatedConversion;
			PagedVoterSnapshot::<T>::iter().count().saturated_into::<PageIndex>()
		}

		pub(crate) fn target_pages() -> PageIndex {
			use sp_runtime::SaturatedConversion;
			PagedTargetSnapshot::<T>::iter().count().saturated_into::<PageIndex>()
		}

		pub(crate) fn voters_iter_flattened() -> impl Iterator<Item = VoterOf<T::MinerConfig>> {
			let key_range =
				(crate::Pallet::<T>::lsp()..=crate::Pallet::<T>::msp()).collect::<Vec<_>>();
			key_range
				.into_iter()
				.flat_map(|k| PagedVoterSnapshot::<T>::get(Self::round(), k).unwrap_or_default())
		}

		pub(crate) fn remove_voter_page(page: PageIndex) {
			PagedVoterSnapshot::<T>::remove(Self::round(), page);
		}

		pub(crate) fn kill_desired_targets() {
			DesiredTargets::<T>::remove(Self::round());
		}

		pub(crate) fn remove_target_page() {
			PagedTargetSnapshot::<T>::remove(Self::round(), Pallet::<T>::msp());
		}

		pub(crate) fn remove_target(at: usize) {
			PagedTargetSnapshot::<T>::mutate(
				Self::round(),
				crate::Pallet::<T>::msp(),
				|maybe_targets| {
					if let Some(targets) = maybe_targets {
						targets.remove(at);
						// and update the hash.
						PagedTargetSnapshotHash::<T>::insert(
							Self::round(),
							crate::Pallet::<T>::msp(),
							T::Hashing::hash(&targets.encode()),
						)
					} else {
						unreachable!();
					}
				},
			)
		}
	}

	/// Desired number of targets to elect for this round.
	#[pallet::storage]
	pub type DesiredTargets<T> = StorageMap<_, Twox64Concat, u32, u32>;
	/// Paginated voter snapshot. At most [`T::Pages`] keys will exist.
	#[pallet::storage]
	pub type PagedVoterSnapshot<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		u32,
		Twox64Concat,
		PageIndex,
		VoterPageOf<T::MinerConfig>,
	>;
	/// Same as [`PagedVoterSnapshot`], but it will store the hash of the snapshot.
	///
	/// The hash is generated using [`frame_system::Config::Hashing`].
	#[pallet::storage]
	pub type PagedVoterSnapshotHash<T: Config> =
		StorageDoubleMap<_, Twox64Concat, u32, Twox64Concat, PageIndex, T::Hash>;
	/// Paginated target snapshot.
	///
	/// For the time being, since we assume one pages of targets, at most ONE key will exist.
	#[pallet::storage]
	pub type PagedTargetSnapshot<T: Config> = StorageDoubleMap<
		_,
		Twox64Concat,
		u32,
		Twox64Concat,
		PageIndex,
		BoundedVec<T::AccountId, T::TargetSnapshotPerBlock>,
	>;
	/// Same as [`PagedTargetSnapshot`], but it will store the hash of the snapshot.
	///
	/// The hash is generated using [`frame_system::Config::Hashing`].
	#[pallet::storage]
	pub type PagedTargetSnapshotHash<T: Config> =
		StorageDoubleMap<_, Twox64Concat, u32, Twox64Concat, PageIndex, T::Hash>;

	#[pallet::pallet]
	pub struct Pallet<T>(PhantomData<T>);
}

impl<T: Config> Pallet<T> {
	/// Returns the most significant page of the snapshot.
	///
	/// Based on the contract of `ElectionDataProvider`, this is the first page that is filled.
	fn msp() -> PageIndex {
		T::Pages::get().checked_sub(1).defensive_unwrap_or_default()
	}

	/// Returns the least significant page of the snapshot.
	///
	/// Based on the contract of `ElectionDataProvider`, this is the last page that is filled.
	fn lsp() -> PageIndex {
		Zero::zero()
	}

	/// > Note: Consider this a shared documentation block for [`Pallet::on_poll`] and this
	/// > function, as they work together.
	///
	/// The meta-phase transition logic that applies to all pallets. Includes the following:
	///
	/// * Creating snapshot once `ElectionProvider::start` has instructed us to do so.
	/// * Transition into `Phase::Signed`.
	/// * Upon last page of `Phase::Signed`, instruct the `Verifier` to start, if any solution
	///   exists.
	/// * And moving forward all other phases as necessary.
	///
	/// What it does not do:
	///
	/// * Instruct the verifier to move forward. This happens through
	///   [`verifier::Verifier::per_block_exec`], called in [`Pallet::on_poll`]. On each block,
	///   [`T::Verifier`] is given a chance to do something. Under the hood, if the `Status` is set,
	///   it will do something, regardless of which phase we are in. In this pallet we only move
	///   [`Phase::SignedValidation`] forward.
	/// * Move us forward if we are in either of `Phase::Done` or `Phase::Export`. These are
	///   controlled by the caller of our [`T::ElectionProvider`] implementation, i.e. staking. Note
	///   that this pallet always transitions us from `current_phase` to `current_phase.next()`, but
	///   the [`crate::types::Phase::next`] function is a noop for `Done` and `Export`.
	///
	/// ### Type
	///
	/// The commonly used `(Weight, Box<dyn Fn(&mut WeightMeter)>)` should be interpreted as such:
	///
	/// * The `Weight` is the pre-computed worst case weight of the operation that we are going to
	///   do.
	/// * The `Box<dyn Fn(&mut WeightMeter)>` is the function that represents that the work that
	///   will at most consume the said amount of weight. While executing, it will alter the given
	///   weight meter to consume the actual weight used. Indeed, the weight that is registered in
	///   the `WeightMeter` must never be more than the `Weight` returned as the first item of the
	///   tuple.
	///
	/// In essence, the caller must:
	///
	/// 1. given an existing `meter`, receive `(worst_weight, exec)`
	/// 2. ensure `meter` can consume up to `worst_weight`.
	/// 3. if so, call `exec(meter)`, knowing `meter` will accumulate at most `worst_weight` extra.
	///
	/// ### Returned Weight
	///
	/// The weights returned are as follows:
	///
	/// * `just_next_phase` returns a benchmarked weight that should be equal to only reading and
	///   writing the `Phase`. This is used for:
	/// 	* `Off` phase, which does not do anything `on_poll`.
	/// 	* `Signed` phase except last page, which starts the verifier.
	/// 	* `Unsigned` phase, which does not do anything `on_poll` and managed its own extrinsic
	///    weights.
	/// 	* `Emergency` phase, which does not do anything `on_poll`.
	/// 	* `Done` phase, which does not do anything `on_poll`.
	/// 	* `SignedValidation` phase, because in this pallet we only move the phase forward and we
	///    don't know (or want to know) how much weight `Verifier` is consuming. This is handled by
	///    combining the weight of [`T::Verifier::per_block_exec`] with this function in
	///    [`Pallet::on_poll`].
	/// 	* `Export` phase, which does not do anything `on_poll` on this pallet, yet we return the
	///    weight that [`Config::ElectionProvider`]'s caller should register via
	///    [`ElectionProvider::status`].
	///
	/// The only special cases, from the perspective of this pallet, are:
	/// 	* Last page of signed phase registers `per_block_start_signed_validation`.
	/// 	* The snapshots are handled by this pallet, which registers `per_block_snapshot_msp` and
	///    `per_block_snapshot_rest`.
	fn per_block_exec(current_phase: Phase<T>) -> (Weight, Box<dyn Fn(&mut WeightMeter)>) {
		type ExecuteFn = Box<dyn Fn(&mut WeightMeter)>;
		let next_phase = current_phase.next();

		let just_next_phase: (Weight, ExecuteFn) = (
			T::WeightInfo::per_block_nothing(),
			Box::new(move |_| {
				// Note `Phase.next` for some variants is a noop, for example `Done`.
				Self::phase_transition(next_phase);
			}),
		);

		match current_phase {
			Phase::Snapshot(x) if x == T::Pages::get() => {
				// first snapshot
				let weight = T::WeightInfo::per_block_snapshot_msp();
				let exec: ExecuteFn = Box::new(move |meter: &mut WeightMeter| {
					Self::create_targets_snapshot();
					Self::phase_transition(next_phase);
					meter.consume(weight)
				});
				(weight, exec)
			},

			Phase::Snapshot(x) => {
				// rest of the snapshot, incl last one.
				let weight = T::WeightInfo::per_block_snapshot_rest();
				let exec: ExecuteFn = Box::new(move |meter: &mut WeightMeter| {
					Self::create_voters_snapshot_paged(x);
					Self::phase_transition(next_phase);
					meter.consume(weight)
				});
				(weight, exec)
			},
			Phase::Signed(x) => {
				// Signed pallet should prep the best winner, and send the start signal, if some
				// exists.
				if x.is_zero() && T::Signed::has_leader(Self::round()) {
					let weight = T::WeightInfo::per_block_start_signed_validation();
					let exec: ExecuteFn = Box::new(move |meter: &mut WeightMeter| {
						// defensive: signed phase has just began, verifier should be in a clear
						// state and ready to accept a solution.
						let _ = T::Verifier::start().defensive();
						Self::phase_transition(next_phase);
						meter.consume(weight)
					});
					(weight, exec)
				} else {
					just_next_phase
				}
			},
			Phase::SignedValidation(_) |
			Phase::Unsigned(_) |
			Phase::Off |
			Phase::Emergency |
			Phase::Done |
			Phase::Export(_) => just_next_phase,
		}
	}

	/// Return the `length` most significant pages.
	///
	/// For example, if `Pages = 4`, and `length = 2`, our full snapshot range would be [0,
	/// 1, 2, 3], with 3 being msp. But, in this case, then this returns `[2, 3]` two most
	/// significant pages, in the old order.
	pub fn msp_range_for(length: usize) -> Vec<PageIndex> {
		(Self::lsp()..Self::msp() + 1).rev().take(length).rev().collect::<Vec<_>>()
	}

	pub(crate) fn phase_transition(to: Phase<T>) {
		let from = Self::current_phase();
		if from == to {
			return;
		}
		use sp_std::mem::discriminant;
		if discriminant(&from) != discriminant(&to) {
			log!(debug, "transitioning phase from {:?} to {:?}", from, to);
			Self::deposit_event(Event::PhaseTransitioned { from, to });
		} else {
			log!(trace, "transitioning phase from {:?} to {:?}", from, to);
		}
		<CurrentPhase<T>>::put(to);
	}

	/// Perform all the basic checks that are independent of the snapshot. To be more specific,
	/// these are all the checks that you can do without the need to read the massive blob of the
	/// actual snapshot. This function only contains a handful of storage reads, with bounded size.
	///
	/// A sneaky detail is that this does check the `DesiredTargets` aspect of the snapshot, but
	/// neither of the large storage items.
	///
	/// Moreover, we do optionally check the fingerprint of the snapshot, if provided.
	///
	/// These complement a feasibility-check, which is exactly the opposite: snapshot-dependent
	/// checks.
	pub(crate) fn snapshot_independent_checks(
		paged_solution: &PagedRawSolution<T::MinerConfig>,
		maybe_snapshot_fingerprint: Option<T::Hash>,
	) -> Result<(), CommonError> {
		// Note that the order of these checks are critical for the correctness and performance of
		// `restore_or_compute_then_maybe_submit`. We want to make sure that we always check round
		// first, so that if it has a wrong round, we can detect and delete it from the cache right
		// from the get go.

		// ensure round is current
		ensure!(Self::round() == paged_solution.round, CommonError::WrongRound);

		// ensure score is being improved, if the claim is even correct.
		ensure!(
			<T::Verifier as Verifier>::ensure_claimed_score_improves(paged_solution.score),
			CommonError::WeakSubmission,
		);

		// ensure solution pages are no more than the snapshot
		ensure!(
			paged_solution.solution_pages.len().saturated_into::<PageIndex>() <= T::Pages::get(),
			CommonError::WrongPageCount
		);

		// finally, check the winner count being correct.
		if let Some(desired_targets) = Snapshot::<T>::desired_targets() {
			ensure!(
				desired_targets == paged_solution.winner_count_single_page_target_snapshot() as u32,
				CommonError::WrongWinnerCount
			)
		}

		// check the snapshot fingerprint, if asked for.
		ensure!(
			maybe_snapshot_fingerprint
				.map_or(true, |snapshot_fingerprint| Snapshot::<T>::fingerprint() ==
					snapshot_fingerprint),
			CommonError::WrongFingerprint
		);

		Ok(())
	}

	/// Creates the target snapshot.
	///
	/// If snapshot creation fails, emits `UnexpectedTargetSnapshotFailed` event
	/// and triggers defensive panic.
	pub(crate) fn create_targets_snapshot() {
		// if requested, get the targets as well.
		let desired_targets = match T::DataProvider::desired_targets() {
			Ok(targets) => targets,
			Err(e) => {
				Self::deposit_event(Event::UnexpectedTargetSnapshotFailed);
				defensive!("Failed to get desired targets: {:?}", e);
				return;
			},
		};
		Snapshot::<T>::set_desired_targets(desired_targets);

		let count = T::TargetSnapshotPerBlock::get();
		let bounds = DataProviderBounds { count: Some(count.into()), size: None };
		let targets: BoundedVec<_, T::TargetSnapshotPerBlock> =
			match T::DataProvider::electable_targets(bounds, 0)
				.and_then(|v| v.try_into().map_err(|_| "try-into failed"))
			{
				Ok(targets) => targets,
				Err(e) => {
					Self::deposit_event(Event::UnexpectedTargetSnapshotFailed);
					defensive!("Failed to create target snapshot: {:?}", e);
					return;
				},
			};

		let count = targets.len() as u32;
		log!(debug, "created target snapshot with {} targets.", count);
		Snapshot::<T>::set_targets(targets);
	}

	/// Creates the voter snapshot.
	///
	/// If snapshot creation fails, emits `UnexpectedVoterSnapshotFailed` event
	/// and triggers defensive panic.
	pub(crate) fn create_voters_snapshot_paged(remaining: PageIndex) {
		let count = T::VoterSnapshotPerBlock::get();
		let bounds = DataProviderBounds { count: Some(count.into()), size: None };
		let voters: BoundedVec<_, T::VoterSnapshotPerBlock> =
			match T::DataProvider::electing_voters(bounds, remaining)
				.and_then(|v| v.try_into().map_err(|_| "try-into failed"))
			{
				Ok(voters) => voters,
				Err(e) => {
					Self::deposit_event(Event::UnexpectedVoterSnapshotFailed);
					defensive!("Failed to create voter snapshot: {:?}", e);
					return;
				},
			};

		let count = voters.len() as u32;
		Snapshot::<T>::set_voters(remaining, voters);
		log!(debug, "created voter snapshot with {} voters, {} remaining.", count, remaining);
	}

	/// Perform the tasks to be done after a new `elect` has been triggered:
	///
	/// 1. Increment round.
	/// 2. Change phase to [`Phase::Off`]
	/// 3. Clear all snapshot data.
	pub(crate) fn rotate_round() {
		// Inc round.
		<Round<T>>::mutate(|r| {
			// Notify the rest of the world
			T::OnRoundRotation::on_round_rotation(*r);
			*r += 1
		});

		// Phase is off now.
		Self::phase_transition(Phase::Off);
	}

	/// Call fallback for the given page.
	///
	/// This uses the [`ElectionProvider::bother`] to check if the fallback is actually going to do
	/// anything. If so, it will re-collect the associated snapshot page and do the fallback. Else,
	/// it will early return without touching the snapshot.
	fn fallback_for_page(page: PageIndex) -> Result<BoundedSupportsOf<Self>, ElectionError<T>> {
		use frame_election_provider_support::InstantElectionProvider;
		let (voters, targets, desired_targets) = if T::Fallback::bother() {
			(
				Snapshot::<T>::voters(page).ok_or(ElectionError::Other("snapshot!"))?,
				Snapshot::<T>::targets().ok_or(ElectionError::Other("snapshot!"))?,
				Snapshot::<T>::desired_targets().ok_or(ElectionError::Other("snapshot!"))?,
			)
		} else {
			(Default::default(), Default::default(), Default::default())
		};
		T::Fallback::instant_elect(voters.into_inner(), targets.into_inner(), desired_targets)
			.map_err(|fe| ElectionError::Fallback(fe))
	}

	/// A reasonable next election block number.
	pub fn average_election_duration() -> u32 {
		let signed: u32 = T::SignedPhase::get().saturated_into();
		let unsigned: u32 = T::UnsignedPhase::get().saturated_into();
		let signed_validation: u32 = T::SignedValidationPhase::get().saturated_into();
		let snapshot = T::Pages::get();

		// we don't count the export.
		let _export = T::Pages::get();

		snapshot + signed + signed_validation + unsigned
	}

	#[cfg(any(test, feature = "runtime-benchmarks", feature = "try-runtime"))]
	pub(crate) fn do_try_state(_: BlockNumberFor<T>) -> Result<(), &'static str> {
		Snapshot::<T>::sanity_check()
	}
}

#[cfg(feature = "std")]
impl<T: Config> Pallet<T> {
	fn analyze_weight(
		op_name: &str,
		op_weight: Weight,
		limit_weight: Weight,
		maybe_max_ratio: Option<sp_runtime::Percent>,
		maybe_max_warn_ratio: Option<sp_runtime::Percent>,
	) {
		use frame_support::weights::constants::{
			WEIGHT_PROOF_SIZE_PER_KB, WEIGHT_REF_TIME_PER_MILLIS,
		};

		let ref_time_ms = op_weight.ref_time() / WEIGHT_REF_TIME_PER_MILLIS;
		let ref_time_ratio =
			sp_runtime::Percent::from_rational(op_weight.ref_time(), limit_weight.ref_time());
		let proof_size_kb = op_weight.proof_size() / WEIGHT_PROOF_SIZE_PER_KB;
		let proof_size_ratio =
			sp_runtime::Percent::from_rational(op_weight.proof_size(), limit_weight.proof_size());
		let limit_ms = limit_weight.ref_time() / WEIGHT_REF_TIME_PER_MILLIS;
		let limit_kb = limit_weight.proof_size() / WEIGHT_PROOF_SIZE_PER_KB;
		log::info!(
			target: crate::LOG_PREFIX,
			"weight of {op_name:?} is: ref-time: {ref_time_ms}ms, {ref_time_ratio:?} of total, proof-size: {proof_size_kb}KiB, {proof_size_ratio:?} of total (total: {limit_ms}ms, {limit_kb}KiB)",
		);

		if let Some(max_ratio) = maybe_max_ratio {
			assert!(ref_time_ratio <= max_ratio && proof_size_ratio <= max_ratio,)
		}
		if let Some(warn_ratio) = maybe_max_warn_ratio {
			if ref_time_ratio > warn_ratio || proof_size_ratio > warn_ratio {
				log::warn!(
					target: crate::LOG_PREFIX,
					"weight of {op_name:?} is above {warn_ratio:?} of the block limit",
				);
			}
		}
	}

	/// Helper function to check the weights of all significant operations of this this pallet
	/// against a runtime.
	///
	/// Will check the weights for:
	///
	/// * snapshot
	/// * signed submission and cleanip
	/// * unsigned solution submission
	/// * signed validation
	/// * export.
	///
	/// Arguments:
	///
	/// * `limit_weight` should be the maximum block weight (often obtained from `frame_system`).
	/// * `maybe_max_ratio` is the maximum ratio of `limit_weight` that we may consume, else we
	///   panic.
	/// * `maybe_max_warn_rati` has the same effect, but it emits a warning instead of panic.
	///
	/// A reasonable value for `maybe_max_weight` would be 75%, and 50% for `maybe_max_warn_ratio`.
	pub fn check_all_weights(
		limit_weight: Weight,
		maybe_max_ratio: Option<sp_runtime::Percent>,
		maybe_max_warn_ratio: Option<sp_runtime::Percent>,
	) where
		T: crate::verifier::Config + crate::signed::Config + crate::unsigned::Config,
	{
		use crate::weights::traits::{
			pallet_election_provider_multi_block_signed::WeightInfo as _,
			pallet_election_provider_multi_block_unsigned::WeightInfo as _,
			pallet_election_provider_multi_block_verifier::WeightInfo as _,
		};

		// -------------- snapshot
		Self::analyze_weight(
			"snapshot_msp",
			<T as Config>::WeightInfo::per_block_snapshot_msp(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		Self::analyze_weight(
			"snapshot_rest",
			<T as Config>::WeightInfo::per_block_snapshot_rest(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		// -------------- signed
		Self::analyze_weight(
			"signed_clear_all_pages",
			<T as crate::signed::Config>::WeightInfo::clear_old_round_data(T::Pages::get()),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);
		Self::analyze_weight(
			"signed_submit_single_pages",
			<T as crate::signed::Config>::WeightInfo::submit_page(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		// -------------- unsigned
		Self::analyze_weight(
			"verify unsigned solution",
			<T as crate::unsigned::Config>::WeightInfo::submit_unsigned(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		// -------------- verification
		Self::analyze_weight(
			"verifier valid terminal",
			<T as crate::verifier::Config>::WeightInfo::verification_valid_terminal(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);
		Self::analyze_weight(
			"verifier invalid terminal",
			<T as crate::verifier::Config>::WeightInfo::verification_invalid_terminal(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		Self::analyze_weight(
			"verifier valid non terminal",
			<T as crate::verifier::Config>::WeightInfo::verification_valid_non_terminal(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		Self::analyze_weight(
			"verifier invalid non terminal",
			<T as crate::verifier::Config>::WeightInfo::verification_invalid_non_terminal(
				T::Pages::get(),
			),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		// -------------- export
		Self::analyze_weight(
			"export non-terminal",
			<T as Config>::WeightInfo::export_non_terminal(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);

		Self::analyze_weight(
			"export terminal",
			<T as Config>::WeightInfo::export_terminal(),
			limit_weight,
			maybe_max_ratio,
			maybe_max_warn_ratio,
		);
	}
}

#[allow(unused)]
#[cfg(any(feature = "runtime-benchmarks", test))]
// helper code for testing and benchmarking
impl<T> Pallet<T>
where
	T: Config + crate::signed::Config + crate::unsigned::Config + crate::verifier::Config,
	BlockNumberFor<T>: From<u32>,
{
	/// Progress blocks until the criteria is met.
	pub(crate) fn roll_until_matches(criteria: impl FnOnce() -> bool + Copy) {
		loop {
			Self::roll_next(false);
			if criteria() {
				break;
			}
		}
	}

	/// Progress blocks until one block before the criteria is met.
	pub(crate) fn roll_until_before_matches(criteria: impl FnOnce() -> bool + Copy) {
		use frame_support::storage::TransactionOutcome;
		loop {
			let should_break = frame_support::storage::with_transaction(
				|| -> TransactionOutcome<Result<_, DispatchError>> {
					Pallet::<T>::roll_next(false);
					if criteria() {
						TransactionOutcome::Rollback(Ok(true))
					} else {
						TransactionOutcome::Commit(Ok(false))
					}
				},
			)
			.unwrap();

			if should_break {
				break;
			}
		}
	}

	pub(crate) fn roll_to_signed_and_mine_full_solution() -> PagedRawSolution<T::MinerConfig> {
		use unsigned::miner::OffchainWorkerMiner;
		Self::roll_to_signed_and_mine_solution(T::Pages::get())
	}

	pub(crate) fn roll_to_signed_and_mine_solution(
		pages: PageIndex,
	) -> PagedRawSolution<T::MinerConfig> {
		use unsigned::miner::OffchainWorkerMiner;
		Self::roll_until_matches(|| Self::current_phase().is_signed());
		// ensure snapshot is full.
		crate::Snapshot::<T>::ensure_full_snapshot().expect("Snapshot is not full");
		OffchainWorkerMiner::<T>::mine_solution(pages, false).expect("mine_solution failed")
	}

	pub(crate) fn submit_full_solution(
		PagedRawSolution { score, solution_pages, .. }: PagedRawSolution<T::MinerConfig>,
	) -> DispatchResultWithPostInfo {
		use frame_system::RawOrigin;
		use sp_std::boxed::Box;
		use types::Pagify;

		// register alice
		let alice = crate::Pallet::<T>::funded_account("alice", 0);
		signed::Pallet::<T>::register(RawOrigin::Signed(alice.clone()).into(), score)?;

		// submit pages
		for (index, page) in solution_pages.pagify(T::Pages::get()) {
			signed::Pallet::<T>::submit_page(
				RawOrigin::Signed(alice.clone()).into(),
				index,
				Some(Box::new(page.clone())),
			)
			.inspect_err(|&e| {
				log!(error, "submit_page {:?} failed: {:?}", page, e);
			})?;
		}

		Ok(().into())
	}

	pub(crate) fn roll_to_signed_and_submit_full_solution() -> DispatchResultWithPostInfo {
		Self::submit_full_solution(Self::roll_to_signed_and_mine_full_solution())
	}

	fn funded_account(seed: &'static str, index: u32) -> T::AccountId {
		use frame_benchmarking::whitelist;
		use frame_support::traits::fungible::{Inspect, Mutate};
		let who: T::AccountId = frame_benchmarking::account(seed, index, 777);
		whitelist!(who);

		// Calculate deposit for worst-case scenario: full queue + all pages submitted.
		// This accounts for the exponential deposit growth in GeometricDepositBase
		// where deposit = base * (1 + increase_factor)^queue_len.
		// We use maximum possible queue_len to ensure adequate funding regardless
		// of queue state changes during benchmark execution.
		let worst_case_deposit = {
			let max_queue_size = T::MaxSubmissions::get() as usize;
			let base = T::DepositBase::calculate_base_deposit(max_queue_size);
			let pages =
				T::DepositPerPage::calculate_page_deposit(max_queue_size, T::Pages::get() as usize);
			base.saturating_add(pages)
		};

		// Transaction fees: assume as conservativ estimate that each operation costs ~1% of
		// minimum_balance
		let min_balance = T::Currency::minimum_balance();
		let num_operations = 1u32.saturating_add(T::Pages::get()); // 1 register + N submit_page
		let tx_fee_buffer = (min_balance / 100u32.into()).saturating_mul(num_operations.into());

		let total_needed = worst_case_deposit
			.saturating_add(tx_fee_buffer)
			.saturating_add(T::Currency::minimum_balance());

		T::Currency::mint_into(&who, total_needed).unwrap();
		who
	}

	/// Roll all pallets forward, for the given number of blocks.
	pub(crate) fn roll_to(n: BlockNumberFor<T>, try_state: bool) {
		let now = frame_system::Pallet::<T>::block_number();
		assert!(n > now, "cannot roll to current or past block");
		let one: BlockNumberFor<T> = 1u32.into();
		let mut i = now + one;
		while i <= n {
			// remove previous weight usage in system.
			frame_system::BlockWeight::<T>::kill();

			frame_system::Pallet::<T>::set_block_number(i);
			let mut meter = frame_system::Pallet::<T>::remaining_block_weight();
			Pallet::<T>::on_poll(i, &mut meter);

			// register the new weight in system
			frame_system::Pallet::<T>::register_extra_weight_unchecked(
				meter.consumed(),
				DispatchClass::Mandatory,
			);

			// invariants must hold at the end of each block.
			if try_state {
				Pallet::<T>::do_try_state(i).unwrap();
				verifier::Pallet::<T>::do_try_state(i).unwrap();
				unsigned::Pallet::<T>::do_try_state(i).unwrap();
				signed::Pallet::<T>::do_try_state(i).unwrap();
			}

			i += one;
		}
	}

	/// Roll to next block.
	pub(crate) fn roll_next(try_state: bool) {
		Self::roll_to(frame_system::Pallet::<T>::block_number() + 1u32.into(), try_state);
	}
}

impl<T: Config> ElectionProvider for Pallet<T> {
	type AccountId = T::AccountId;
	type BlockNumber = BlockNumberFor<T>;
	type Error = ElectionError<T>;
	type DataProvider = T::DataProvider;
	type Pages = T::Pages;
	type MaxWinnersPerPage = <T::Verifier as Verifier>::MaxWinnersPerPage;
	type MaxBackersPerWinner = <T::Verifier as Verifier>::MaxBackersPerWinner;
	type MaxBackersPerWinnerFinal = <T::Verifier as Verifier>::MaxBackersPerWinnerFinal;

	fn elect(remaining: PageIndex) -> Result<BoundedSupportsOf<Self>, Self::Error> {
		match Self::status() {
			// we allow `elect` to be called as long as we have received a start signal.
			Ok(_) => (),
			Err(_) => return Err(ElectionError::NotOngoing),
		}

		let current_phase = CurrentPhase::<T>::get();
		if let Phase::Export(expected) = current_phase {
			ensure!(expected == remaining, ElectionError::OutOfOrder);
		}

		let result = T::Verifier::get_queued_solution_page(remaining)
			.ok_or(ElectionError::SupportPageNotAvailable)
			.or_else(|err: ElectionError<T>| {
				log!(
					debug,
					"primary election for page {} failed due to: {:?}, trying fallback",
					remaining,
					err,
				);
				Self::fallback_for_page(remaining)
			})
			.map_err(|err| {
				// if any pages returns an error, we go into the emergency phase and don't do
				// anything else anymore. This will prevent any new submissions to signed and
				// unsigned pallet, and thus the verifier will also be almost stuck, except for the
				// submission of emergency solutions.
				log!(debug, "fallback also ({:?}) failed for page {:?}", err, remaining);
				err
			})
			.map(|supports| {
				// convert to bounded
				supports.into()
			});

		// if fallback has possibly put us into the emergency phase, don't do anything else.
		if CurrentPhase::<T>::get().is_emergency() && result.is_err() {
			log!(error, "Emergency phase triggered, halting the election.");
		} else {
			if remaining.is_zero() {
				Self::rotate_round()
			} else {
				Self::phase_transition(Phase::Export(remaining - 1))
			}
		}

		result
	}

	fn start() -> Result<(), Self::Error> {
		match Self::status() {
			Err(()) => (),
			Ok(_) => return Err(ElectionError::Ongoing),
		}

		Self::phase_transition(Phase::<T>::start_phase());
		Ok(())
	}

	fn duration() -> Self::BlockNumber {
		Self::average_election_duration().into()
	}

	fn status() -> Result<Option<Weight>, ()> {
		match <CurrentPhase<T>>::get() {
			// we're not doing anything.
			Phase::Off => Err(()),

			// we're doing something but not ready.
			Phase::Signed(_) |
			Phase::SignedValidation(_) |
			Phase::Unsigned(_) |
			Phase::Snapshot(_) |
			Phase::Emergency => Ok(None),

			// we're ready
			Phase::Done => Ok(Some(T::WeightInfo::export_non_terminal())),
			Phase::Export(p) => {
				if p.is_zero() {
					Ok(Some(T::WeightInfo::export_terminal()))
				} else {
					Ok(Some(T::WeightInfo::export_non_terminal()))
				}
			},
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn asap() {
		// prepare our snapshot so we can "hopefully" run a fallback.
		Self::create_targets_snapshot();
		for p in (Self::lsp()..=Self::msp()).rev() {
			Self::create_voters_snapshot_paged(p)
		}
	}
}

#[cfg(test)]
mod phase_rotation {
	use super::{Event, *};
	use crate::{mock::*, verifier::Status, Phase};
	use frame_election_provider_support::ElectionProvider;
	use frame_support::assert_ok;

	#[test]
	fn single_page() {
		ExtBuilder::full()
			.pages(1)
			.election_start(13)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				// 0 -------- 14 15 --------- 20 ------------- 25 ---------- 30
				//            |  |            |                |             |
				//    Snapshot Signed  SignedValidation    Unsigned       elect()

				assert_eq!(System::block_number(), 0);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 1));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(4);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_eq!(MultiBlock::round(), 0);

				roll_to(13);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(1));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 3));

				roll_to(14);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 0));

				roll_to(15);
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(SignedPhase::get() - 1));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 1));
				assert_eq!(MultiBlock::round(), 0);

				assert_eq!(
					multi_block_events_since_last_call(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(1) },
						Event::PhaseTransitioned {
							from: Phase::Snapshot(0),
							to: Phase::Signed(SignedPhase::get() - 1)
						}
					]
				);

				roll_to(19);
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(0));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(20);
				assert_eq!(
					MultiBlock::current_phase(),
					Phase::SignedValidation(SignedValidationPhase::get())
				);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 1));
				assert_eq!(MultiBlock::round(), 0);

				assert_eq!(
					multi_block_events_since_last_call(),
					vec![Event::PhaseTransitioned {
						from: Phase::Signed(0),
						to: Phase::SignedValidation(SignedValidationPhase::get())
					}],
				);

				roll_to(26);
				assert_eq!(MultiBlock::current_phase(), Phase::SignedValidation(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 1));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(27);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(UnsignedPhase::get() - 1));
				assert_eq!(
					multi_block_events_since_last_call(),
					vec![Event::PhaseTransitioned {
						from: Phase::SignedValidation(0),
						to: Phase::Unsigned(UnsignedPhase::get() - 1)
					}],
				);

				roll_to(31);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(0));

				// We stay in done otherwise
				roll_to(32);
				assert!(MultiBlock::current_phase().is_done());

				// We stay in done otherwise
				roll_to(33);
				assert!(MultiBlock::current_phase().is_done());

				// We close when upstream tells us to elect.
				roll_to(34);
				assert!(MultiBlock::current_phase().is_done());
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 1));

				MultiBlock::elect(0).unwrap();

				assert!(MultiBlock::current_phase().is_off());
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 1));
				assert_eq!(MultiBlock::round(), 1);

				roll_to(42);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
			})
	}

	#[test]
	fn multi_page_2() {
		ExtBuilder::full()
			.pages(2)
			.fallback_mode(FallbackModes::Onchain)
			.election_start(12)
			.build_and_execute(|| {
				// 0 -------13 14 15 ------- 20 ---- 25 ------- 30
				//           |     |         |       |          |
				//    Snapshot    Signed SigValid  Unsigned   Elect

				assert_eq!(System::block_number(), 0);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 2));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(4);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_eq!(MultiBlock::round(), 0);

				roll_to(11);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_eq!(MultiBlock::round(), 0);

				roll_to(12);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(2));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 2));

				roll_to(13);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(1));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 0));

				roll_to(14);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 1));

				roll_to(15);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));
				assert_eq!(MultiBlock::round(), 0);
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(SignedPhase::get() - 1));

				assert_eq!(
					multi_block_events_since_last_call(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(2) },
						Event::PhaseTransitioned {
							from: Phase::Snapshot(0),
							to: Phase::Signed(SignedPhase::get() - 1)
						}
					]
				);

				roll_to(19);
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(20);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));
				assert_eq!(MultiBlock::round(), 0);
				assert_eq!(
					MultiBlock::current_phase(),
					Phase::SignedValidation(SignedValidationPhase::get())
				);

				assert_eq!(
					multi_block_events_since_last_call(),
					vec![Event::PhaseTransitioned {
						from: Phase::Signed(0),
						to: Phase::SignedValidation(SignedValidationPhase::get())
					}],
				);

				roll_to(26);
				assert_eq!(MultiBlock::current_phase(), Phase::SignedValidation(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(27);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(UnsignedPhase::get() - 1));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));
				assert_eq!(MultiBlock::round(), 0);

				assert_eq!(
					multi_block_events_since_last_call(),
					vec![Event::PhaseTransitioned {
						from: Phase::SignedValidation(0),
						to: Phase::Unsigned(UnsignedPhase::get() - 1)
					}],
				);

				roll_to(31);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));

				roll_to(32);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));

				// We close when upstream tells us to elect.
				roll_to(33);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				// and even this one's coming from the fallback.
				MultiBlock::elect(0).unwrap();
				assert!(MultiBlock::current_phase().is_off());

				// all snapshots are gone.
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 2));
				assert_eq!(MultiBlock::round(), 1);
			})
	}

	#[test]
	fn multi_page_3() {
		ExtBuilder::full()
			.pages(3)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				// 0 ------- 12 13 14 15 ----------- 20 ---------25 ------- 30
				//            |       |              |            |          |
				//     Snapshot      Signed   SignedValidation  Unsigned   Elect

				assert_eq!(System::block_number(), 0);
				assert!(MultiBlock::current_phase().is_off());
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 3));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(10);
				assert!(MultiBlock::current_phase().is_off());
				assert_eq!(MultiBlock::round(), 0);

				roll_to(11);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(3));
				// no snapshot is take yet, we start at the next block
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(false, 3));

				roll_to(12);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(2));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 0));

				roll_to(13);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(1));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 1));

				roll_to(14);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(0));
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, 2));

				roll_to(15);
				assert_ok!(Snapshot::<Runtime>::ensure_snapshot(true, Pages::get()));
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(4));
				assert_eq!(
					multi_block_events_since_last_call(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
						Event::PhaseTransitioned {
							from: Phase::Snapshot(0),
							to: Phase::Signed(SignedPhase::get() - 1)
						}
					]
				);
				assert_eq!(MultiBlock::round(), 0);

				roll_to(19);
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(0));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(20);
				assert_eq!(
					MultiBlock::current_phase(),
					Phase::SignedValidation(SignedValidationPhase::get())
				);
				assert_eq!(
					multi_block_events_since_last_call(),
					vec![Event::PhaseTransitioned {
						from: Phase::Signed(0),
						to: Phase::SignedValidation(SignedValidationPhase::get())
					}]
				);

				roll_to(26);
				assert_eq!(MultiBlock::current_phase(), Phase::SignedValidation(0));
				assert_eq!(MultiBlock::round(), 0);

				roll_to(27);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(UnsignedPhase::get() - 1));
				assert_eq!(
					multi_block_events_since_last_call(),
					vec![Event::PhaseTransitioned {
						from: Phase::SignedValidation(0),
						to: Phase::Unsigned(UnsignedPhase::get() - 1)
					}]
				);

				roll_to(31);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(0));

				roll_to(32);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				// We close when upstream tells us to elect.
				roll_to(33);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				MultiBlock::elect(0).unwrap();
				assert!(MultiBlock::current_phase().is_off());

				// all snapshots are gone.
				assert_none_snapshot();
				assert_eq!(MultiBlock::round(), 1);
			})
	}

	#[test]
	fn weights_registered() {
		// ensure we never forget to call `meter.consume` or similar in poll and alike.
		// Our mock setup is:
		//
		// * each db read or write is 1 ref time.
		// * each epmb op weight are:
		//   * snapshots: 5
		//   * validation: 3 to start, rest 7
		ExtBuilder::full().build_and_execute(|| {
			roll_to(10);
			assert!(MultiBlock::current_phase().is_off());
			// note: 2 becuase 1 read registered by the parent pallet, 1 by verifier.
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 0));

			// roll to this phase, no weight meter is consumed yet other than 1 read + 1 write.
			roll_next_and_phase(Phase::Snapshot(3));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 0));

			roll_next_and_phase(Phase::Snapshot(2));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 5));

			roll_next_and_phase(Phase::Snapshot(1));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 5));

			roll_next_and_phase(Phase::Snapshot(0));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 5));

			roll_next_and_phase(Phase::Signed(SignedPhase::get() - 1));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 5));

			// Now snapshot is done, and during signed phase we do a noop.
			roll_next_and_phase(Phase::Signed(SignedPhase::get() - 2));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 0));

			// but let's submit a signed solution to be verified while we're here
			{
				let paged = mine_full_solution().unwrap();
				load_signed_for_verification(999, paged.clone());
			}

			// let's go forward to start of signed validation
			roll_to_signed_validation_open();
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 3));

			roll_next_and_phase_verifier(
				Phase::SignedValidation(SignedValidationPhase::get() - 1),
				Status::Ongoing(1),
			);
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(1, 7));

			roll_next_and_phase_verifier(
				Phase::SignedValidation(SignedValidationPhase::get() - 2),
				Status::Ongoing(0),
			);
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(1, 7));

			roll_next_and_phase_verifier(
				Phase::SignedValidation(SignedValidationPhase::get() - 3),
				Status::Nothing,
			);
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(1, 7));

			// we also don't do anything during unsigned phase.
			roll_to_unsigned_open();
			assert!(MultiBlock::current_phase().is_unsigned());
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 0));

			roll_next_and_phase(Phase::Unsigned(UnsignedPhase::get() - 2));
			assert_eq!(System::remaining_block_weight().consumed(), Weight::from_parts(2, 0));

			// Export weight is computed by us, but registered by whoever calls `elect`, not our
			// business to check.
		});
	}

	#[test]
	fn no_unsigned_phase() {
		ExtBuilder::full()
			.pages(3)
			.unsigned_phase(0)
			.election_start(16)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				// 0 --------------------- 17 ------ 20 ---------25 ------- 30
				//            |            |         |            |          |
				//                     Snapshot    Signed  SignedValidation   Elect

				assert_eq!(System::block_number(), 0);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_none_snapshot();
				assert_eq!(MultiBlock::round(), 0);

				roll_to(4);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_eq!(MultiBlock::round(), 0);

				roll_to(16);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(3));

				roll_to(17);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(2));

				roll_to(18);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(1));

				roll_to(19);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(0));

				roll_to(20);
				assert_eq!(MultiBlock::current_phase(), Phase::Signed(SignedPhase::get() - 1));

				assert_full_snapshot();
				assert_eq!(MultiBlock::round(), 0);

				roll_to(25);
				assert_eq!(
					MultiBlock::current_phase(),
					Phase::SignedValidation(SignedValidationPhase::get())
				);

				assert_eq!(
					multi_block_events_since_last_call(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
						Event::PhaseTransitioned {
							from: Phase::Snapshot(0),
							to: Phase::Signed(SignedPhase::get() - 1)
						},
						Event::PhaseTransitioned {
							from: Phase::Signed(0),
							to: Phase::SignedValidation(SignedValidationPhase::get())
						},
					]
				);

				// last block of signed validation
				roll_to(31);
				assert_eq!(MultiBlock::current_phase(), Phase::SignedValidation(0));

				// we are done now
				roll_to(32);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				roll_to(33);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				MultiBlock::elect(0).unwrap();
				assert!(MultiBlock::current_phase().is_off());

				// all snapshots are gone.
				assert_none_snapshot();
				assert_eq!(MultiBlock::round(), 1);
				assert_ok!(signed::Submissions::<Runtime>::ensure_killed(0));
				verifier::QueuedSolution::<Runtime>::assert_killed();
			})
	}

	#[test]
	fn no_signed_phase() {
		ExtBuilder::full()
			.pages(3)
			.signed_phase(0, 0)
			.election_start(21)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				// 0 ------------------------- 22 ------ 25 ------- 30
				//                             |         |          |
				//                         Snapshot   Unsigned   Elect

				assert_eq!(System::block_number(), 0);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_none_snapshot();
				assert_eq!(MultiBlock::round(), 0);

				roll_to(20);
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
				assert_eq!(MultiBlock::round(), 0);

				roll_to(21);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(3));
				roll_to(22);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(2));
				roll_to(23);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(1));
				roll_to(24);
				assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(0));

				roll_to(25);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(UnsignedPhase::get() - 1));
				assert_full_snapshot();
				assert_eq!(MultiBlock::round(), 0);

				assert_eq!(
					multi_block_events(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
						Event::PhaseTransitioned {
							from: Phase::Snapshot(0),
							to: Phase::Unsigned(UnsignedPhase::get() - 1)
						},
					]
				);

				roll_to(29);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(0));

				roll_to(30);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);
				roll_to(31);
				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				// eventually the call to elect comes, and we exit done phase.
				MultiBlock::elect(0).unwrap();
				assert!(MultiBlock::current_phase().is_off());

				// all snapshots are gone.
				assert_none_snapshot();
				assert_eq!(MultiBlock::round(), 1);
				assert_ok!(signed::Submissions::<Runtime>::ensure_killed(0));
				verifier::QueuedSolution::<Runtime>::assert_killed();
			})
	}

	#[test]
	#[should_panic(expected = "either signed or unsigned phase must be set")]
	fn no_signed_and_unsigned_phase() {
		ExtBuilder::full()
			.pages(3)
			.signed_phase(0, 0)
			.unsigned_phase(0)
			.election_start(10)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				// This should panic during integrity test
			});
	}

	#[test]
	#[should_panic(
		expected = "signed validation phase should be a multiple of the number of pages."
	)]
	fn incorrect_signed_validation_phase_shorter_than_number_of_pages() {
		ExtBuilder::full().pages(3).signed_validation_phase(2).build_and_execute(|| {})
	}

	#[test]
	#[should_panic(
		expected = "signed validation phase should be a multiple of the number of pages."
	)]
	fn incorret_signed_validation_phase_not_a_multiple_of_the_number_of_pages() {
		ExtBuilder::full().pages(3).signed_validation_phase(7).build_and_execute(|| {})
	}

	#[test]
	fn are_we_done_back_to_signed() {
		ExtBuilder::full()
			.are_we_done(AreWeDoneModes::BackToSigned)
			.build_and_execute(|| {
				// roll to unsigned
				roll_to_last_unsigned();

				assert_eq!(MultiBlock::round(), 0);
				assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(0));
				assert_eq!(
					multi_block_events_since_last_call(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
						Event::PhaseTransitioned { from: Phase::Snapshot(0), to: Phase::Signed(4) },
						Event::PhaseTransitioned {
							from: Phase::Signed(0),
							to: Phase::SignedValidation(SignedValidationPhase::get())
						},
						Event::PhaseTransitioned {
							from: Phase::SignedValidation(0),
							to: Phase::Unsigned(4)
						}
					]
				);

				// we are back to signed phase
				roll_next_and_phase(Phase::Signed(SignedPhase::get() - 1));
				// round is still the same
				assert_eq!(MultiBlock::round(), 0);

				// we proceed to normally again:
				roll_next_and_phase(Phase::Signed(SignedPhase::get() - 2));
				roll_next_and_phase(Phase::Signed(SignedPhase::get() - 3));
			});
	}

	#[test]
	fn export_phase_only_transitions_on_elect() {
		ExtBuilder::full()
			.pages(3)
			.election_start(13)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				roll_to_done();

				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				// Test that on_initialize does NOT advance the phase when in Done
				roll_next_and_phase(Phase::Done);

				// Start export by calling elect(max_page)
				assert_ok!(MultiBlock::elect(2)); // max_page = 2 for 3 pages
				assert_eq!(MultiBlock::current_phase(), Phase::Export(1));

				// Test that on_initialize does NOT advance the phase when in Export
				roll_next_and_phase(Phase::Export(1));

				// Only elect() should advance the Export phase
				assert_ok!(MultiBlock::elect(1));
				assert_eq!(MultiBlock::current_phase(), Phase::Export(0));

				// Test Export(0) also blocks on_initialize transitions
				roll_next_and_phase(Phase::Export(0));

				// Complete the export manually
				assert_ok!(MultiBlock::elect(0));
				assert_eq!(MultiBlock::current_phase(), Phase::Off);
			});
	}

	#[test]
	fn export_phase_out_of_order_elect_fails() {
		ExtBuilder::full()
			.pages(3)
			.election_start(13)
			.fallback_mode(FallbackModes::Onchain)
			.build_and_execute(|| {
				roll_to_done();

				assert_eq!(MultiBlock::current_phase(), Phase::Done);

				// Start export by calling elect(max_page)
				assert_ok!(MultiBlock::elect(2)); // max_page = 2 for 3 pages
				assert_eq!(MultiBlock::current_phase(), Phase::Export(1));

				// Out of order: try to call elect(2) again, should fail
				assert_eq!(MultiBlock::elect(2), Err(ElectionError::OutOfOrder));

				// Out of order: try to call elect(0) before elect(1), should fail
				assert_eq!(MultiBlock::elect(0), Err(ElectionError::OutOfOrder));

				// Correct order: elect(1) works
				assert_ok!(MultiBlock::elect(1));
				assert_eq!(MultiBlock::current_phase(), Phase::Export(0));
			});
	}

	#[test]
	#[cfg_attr(debug_assertions, should_panic(expected = "Defensive failure has been triggered!"))]
	fn target_snapshot_failed_event_emitted() {
		ExtBuilder::full()
				.pages(2)
				.election_start(13)
				.build_and_execute(|| {
					// Create way more targets than the TargetSnapshotPerBlock limit (4)
					// This will cause bounds.slice_exhausted(&targets) to return true
					let too_many_targets: Vec<AccountId> = (1..=100).collect();
					Targets::set(too_many_targets);

					roll_to(13);
					assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(2));

					// Clear any existing events
					let _ = multi_block_events_since_last_call();

					// Roll to next block - on_initialize will be in Phase::Snapshot(2) where x == T::Pages::get()
					// This triggers target snapshot creation, which should fail due to too many targets
					roll_to(14);

					// Verify that UnexpectedTargetSnapshotFailed event was emitted
					let events = multi_block_events_since_last_call();
					assert!(
						events.contains(&Event::UnexpectedTargetSnapshotFailed),
						"UnexpectedTargetSnapshotFailed event should have been emitted when target snapshot creation fails. Events: {:?}",
						events
					);

					// Verify phase transition still happened despite the failure
					assert_eq!(MultiBlock::current_phase(), Phase::Snapshot(1));
				});
	}
}

#[cfg(test)]
mod election_provider {
	use super::*;
	use crate::{
		mock::*,
		unsigned::miner::OffchainWorkerMiner,
		verifier::{AsynchronousVerifier, Status, Verifier},
		Phase,
	};
	use frame_election_provider_support::{BoundedSupport, BoundedSupports, ElectionProvider};
	use frame_support::{assert_storage_noop, testing_prelude::bounded_vec};

	// This is probably the most important test of all, a basic, correct scenario. This test should
	// be studied in detail, and all of the branches of how it can go wrong or diverge from the
	// basic scenario assessed.
	#[test]
	fn multi_page_elect_simple_works() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			assert!(MultiBlock::current_phase().is_signed());

			// load a solution into the verifier
			let paged = OffchainWorkerMiner::<Runtime>::mine_solution(Pages::get(), false).unwrap();
			let score = paged.score;

			// now let's submit this one by one, into the signed phase.
			load_signed_for_verification(99, paged);

			// now the solution should start being verified.
			roll_to_signed_validation_open();

			assert_eq!(
				multi_block_events(),
				vec![
					Event::PhaseTransitioned {
						from: Phase::Off,
						to: Phase::Snapshot(Pages::get())
					},
					Event::PhaseTransitioned {
						from: Phase::Snapshot(0),
						to: Phase::Signed(SignedPhase::get() - 1)
					},
					Event::PhaseTransitioned {
						from: Phase::Signed(0),
						to: Phase::SignedValidation(SignedValidationPhase::get())
					}
				]
			);
			assert_eq!(verifier_events_since_last_call(), vec![]);

			// there is no queued solution prior to the last page of the solution getting verified
			assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), None);
			assert_eq!(
				<Runtime as crate::Config>::Verifier::status(),
				verifier::Status::Ongoing(2)
			);

			// next block, signed will start the verifier, although nothing is verified yet.
			roll_next_and_phase_verifier(Phase::SignedValidation(5), Status::Ongoing(1));
			assert_eq!(verifier_events_since_last_call(), vec![verifier::Event::Verified(2, 2)]);

			roll_next_and_phase_verifier(Phase::SignedValidation(4), Status::Ongoing(0));
			assert_eq!(verifier_events_since_last_call(), vec![verifier::Event::Verified(1, 2)]);

			roll_next_and_phase_verifier(Phase::SignedValidation(3), Status::Nothing);
			assert_eq!(
				verifier_events_since_last_call(),
				vec![verifier::Event::Verified(0, 2), verifier::Event::Queued(score, None)]
			);

			// there is now a queued solution.
			assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), Some(score));

			// now let's go to unsigned phase, but we don't expect anything to happen there since we
			// don't run OCWs.
			roll_to_unsigned_open();

			// pre-elect state
			assert!(MultiBlock::current_phase().is_unsigned_opened_now());
			assert_eq!(MultiBlock::round(), 0);
			assert_full_snapshot();

			// call elect for each page
			let _paged_solution = (MultiBlock::lsp()..MultiBlock::msp())
				.rev() // 2, 1, 0
				.map(|page| {
					MultiBlock::elect(page as PageIndex).unwrap();
					if page == 0 {
						assert!(MultiBlock::current_phase().is_off())
					} else {
						assert_eq!(MultiBlock::current_phase(), Phase::Export(page - 1))
					}
				})
				.collect::<Vec<_>>();

			// after the last elect, verifier is cleared,
			verifier::QueuedSolution::<Runtime>::assert_killed();
			// the phase is off,
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
			// the round is incremented,
			assert_eq!(Round::<Runtime>::get(), 1);
			// and the snapshot is cleared,
			assert_storage_noop!(Snapshot::<Runtime>::kill());
			// signed pallet is clean.
			// NOTE: signed pallet lazily deletes all other solutions, except the winner, which is
			// actually deleted.
			assert_ok!(signed::Submissions::<Runtime>::ensure_killed(0));
		});
	}

	#[test]
	fn multi_page_elect_fast_track() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			let round = MultiBlock::round();
			assert!(MultiBlock::current_phase().is_signed());

			// load a solution into the verifier
			let paged = OffchainWorkerMiner::<Runtime>::mine_solution(Pages::get(), false).unwrap();
			let score = paged.score;
			load_signed_for_verification_and_start(99, paged, 0);

			// there is no queued solution prior to the last page of the solution getting verified
			assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), None);

			// roll to the block it is finalized.
			roll_next_and_phase_verifier(Phase::SignedValidation(5), Status::Ongoing(1));
			roll_next_and_phase_verifier(Phase::SignedValidation(4), Status::Ongoing(0));
			roll_next_and_phase_verifier(Phase::SignedValidation(3), Status::Nothing);

			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					verifier::Event::Verified(2, 2),
					verifier::Event::Verified(1, 2),
					verifier::Event::Verified(0, 2),
					verifier::Event::Queued(score, None),
				]
			);

			// there is now a queued solution.
			assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), Some(score));

			// not much impact, just for the sane-ness of the test.
			roll_to_unsigned_open();

			// pre-elect state:
			assert!(MultiBlock::current_phase().is_unsigned_opened_now());
			assert_eq!(Round::<Runtime>::get(), 0);
			assert_full_snapshot();

			// there are 3 pages (indexes 2..=0), but we short circuit by just calling 0.
			let _supports = crate::Pallet::<Runtime>::elect(0).unwrap();

			// round is incremented.
			assert_eq!(MultiBlock::round(), round + 1);
			// after elect(0) is called, verifier is cleared,
			verifier::QueuedSolution::<Runtime>::assert_killed();
			// the phase is off,
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
			// the round is incremented,
			assert_eq!(Round::<Runtime>::get(), 1);
			// the snapshot is cleared,
			assert_none_snapshot();
			// and signed pallet is clean.
			assert_ok!(signed::Submissions::<Runtime>::ensure_killed(round));
		});
	}

	#[test]
	fn elect_does_not_finish_without_call_of_page_0() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			assert!(MultiBlock::current_phase().is_signed());

			// load a solution into the verifier
			let paged = OffchainWorkerMiner::<Runtime>::mine_solution(Pages::get(), false).unwrap();
			let score = paged.score;
			load_signed_for_verification_and_start(99, paged, 0);

			// there is no queued solution prior to the last page of the solution getting verified
			assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), None);

			// roll to the block it is finalized. 1 block to start the verifier, and 3 to verify.
			roll_next_and_phase_verifier(Phase::SignedValidation(5), Status::Ongoing(1));
			roll_next_and_phase_verifier(Phase::SignedValidation(4), Status::Ongoing(0));
			roll_next_and_phase_verifier(Phase::SignedValidation(3), Status::Nothing);

			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					verifier::Event::Verified(2, 2),
					verifier::Event::Verified(1, 2),
					verifier::Event::Verified(0, 2),
					verifier::Event::Queued(score, None),
				]
			);

			// there is now a queued solution
			assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), Some(score));

			// not much impact, just for the sane-ness of the test.
			roll_to_unsigned_open();

			// pre-elect state:
			assert!(MultiBlock::current_phase().is_unsigned_opened_now());
			assert_eq!(Round::<Runtime>::get(), 0);
			assert_full_snapshot();

			// call elect for page 2 and 1, but NOT 0
			let solutions = (1..=MultiBlock::msp())
				.rev() // 2, 1
				.map(|page| {
					crate::Pallet::<Runtime>::elect(page as PageIndex).unwrap();
					assert!(MultiBlock::current_phase().is_export());
				})
				.collect::<Vec<_>>();
			assert_eq!(solutions.len(), 2);

			// nothing changes from the prelect state, except phase is now export.
			assert!(MultiBlock::current_phase().is_export());
			assert_eq!(Round::<Runtime>::get(), 0);
			assert_full_snapshot();
		});
	}

	#[test]
	fn continue_fallback_works() {
		// Use Continue fallback to avoid emergency phase when both primary and fallback fail.
		ExtBuilder::full().fallback_mode(FallbackModes::Continue).build_and_execute(|| {
			// Move to unsigned phase
			roll_to_unsigned_open();

			// Note: our mock runtime is configured with 1 page for the unsigned phase
			let miner_pages = <Runtime as unsigned::Config>::MinerPages::get();
			// Mine an unsigned solution
			let unsigned_solution =
				OffchainWorkerMiner::<Runtime>::mine_solution(miner_pages, true).unwrap();

			// Submit the unsigned solution
			assert_ok!(UnsignedPallet::submit_unsigned(
				RuntimeOrigin::none(),
				Box::new(unsigned_solution)
			));

			// Move to Done phase
			roll_to_done();

			// In the mock runtime Pages::get() = 3 so unsigned solution has 1 page but we go
			// through Done -> Export(2) -> Export(1) -> Export(0) -> Off via elect().
			// First elect call should succeed
			let result1 = MultiBlock::elect(2);
			assert!(result1.is_ok());
			assert_eq!(MultiBlock::current_phase(), Phase::Export(1));

			// Second elect call should now fail because we have mined a solution with MinerPages
			// equal to 1.
			// Phase should advance even on error.
			let result2 = MultiBlock::elect(1);
			assert!(result2.is_err());
			assert_eq!(MultiBlock::current_phase(), Phase::Export(0));

			// Third elect call should also fail but still advance to Off
			let result3 = MultiBlock::elect(0);
			assert!(result3.is_err());
			assert!(matches!(MultiBlock::current_phase(), Phase::Off));
		});
	}

	#[test]
	fn skip_unsigned_phase() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			assert!(MultiBlock::current_phase().is_signed());
			let round = MultiBlock::round();

			// load a solution into the verifier
			let paged = OffchainWorkerMiner::<Runtime>::mine_solution(Pages::get(), false).unwrap();

			load_signed_for_verification_and_start_and_roll_to_verified(99, paged, 0);

			// and right here, in the middle of the signed verification phase, we close the round.
			// Everything should work fine.
			assert!(matches!(MultiBlock::current_phase(), Phase::SignedValidation(_)));
			assert_eq!(Round::<Runtime>::get(), 0);
			assert_full_snapshot();

			// fetch all pages.
			let _paged_solution = (MultiBlock::lsp()..MultiBlock::msp())
				.rev() // 2, 1, 0
				.map(|page| {
					MultiBlock::elect(page as PageIndex).unwrap();
					if page == 0 {
						assert!(MultiBlock::current_phase().is_off())
					} else {
						assert!(MultiBlock::current_phase().is_export())
					}
				})
				.collect::<Vec<_>>();

			// round is incremented.
			assert_eq!(MultiBlock::round(), round + 1);
			// after elect(0) is called, verifier is cleared,
			verifier::QueuedSolution::<Runtime>::assert_killed();
			// the phase is off,
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
			// the snapshot is cleared,
			assert_none_snapshot();
			// and signed pallet is clean.
			assert_ok!(signed::Submissions::<Runtime>::ensure_killed(round));
		});
	}

	#[test]
	#[allow(deprecated)]
	fn call_to_elect_should_prevent_any_submission() {
		use frame_support::unsigned::ValidateUnsigned;
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			assert!(MultiBlock::current_phase().is_signed());

			// load a solution into the verifier
			let paged = OffchainWorkerMiner::<Runtime>::mine_solution(Pages::get(), false).unwrap();
			load_signed_for_verification_and_start_and_roll_to_verified(99, paged, 0);

			assert!(matches!(MultiBlock::current_phase(), Phase::SignedValidation(_)));

			// fetch one page.
			assert!(MultiBlock::elect(MultiBlock::msp()).is_ok());

			// try submit one signed page:
			assert_noop!(
				SignedPallet::submit_page(RuntimeOrigin::signed(999), 0, Default::default()),
				crate::signed::Error::<Runtime>::PhaseNotSigned,
			);
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(999), Default::default()),
				crate::signed::Error::<Runtime>::PhaseNotSigned,
			);
			assert_storage_noop!(assert!(<UnsignedPallet as ValidateUnsigned>::pre_dispatch(
				&unsigned::Call::submit_unsigned { paged_solution: Default::default() }
			)
			.is_err()));
		});
	}

	#[test]
	fn multi_page_onchain_elect_fallback_works() {
		ExtBuilder::full().fallback_mode(FallbackModes::Onchain).build_and_execute(|| {
			roll_to_signed_open();

			// same targets, but voters from page 2 (1, 2, 3, 4, see `mock/staking`).
			assert_eq!(
				MultiBlock::elect(2).unwrap(),
				BoundedSupports(bounded_vec![
					(10, BoundedSupport { total: 15, voters: bounded_vec![(1, 10), (4, 5)] }),
					(
						40,
						BoundedSupport {
							total: 25,
							voters: bounded_vec![(2, 10), (3, 10), (4, 5)]
						}
					)
				])
			);
			// page 1 of voters
			assert_eq!(
				MultiBlock::elect(1).unwrap(),
				BoundedSupports(bounded_vec![
					(10, BoundedSupport { total: 15, voters: bounded_vec![(5, 5), (8, 10)] }),
					(
						30,
						BoundedSupport {
							total: 25,
							voters: bounded_vec![(5, 5), (6, 10), (7, 10)]
						}
					)
				])
			);
			// self votes
			assert_eq!(
				MultiBlock::elect(0).unwrap(),
				BoundedSupports(bounded_vec![
					(30, BoundedSupport { total: 30, voters: bounded_vec![(30, 30)] }),
					(40, BoundedSupport { total: 40, voters: bounded_vec![(40, 40)] })
				])
			);

			assert_eq!(
				multi_block_events(),
				vec![
					Event::PhaseTransitioned {
						from: Phase::Off,
						to: Phase::Snapshot(Pages::get())
					},
					Event::PhaseTransitioned {
						from: Phase::Snapshot(0),
						to: Phase::Signed(SignedPhase::get() - 1)
					},
					Event::PhaseTransitioned {
						from: Phase::Signed(SignedPhase::get() - 1),
						to: Phase::Export(1)
					},
					Event::PhaseTransitioned { from: Phase::Export(0), to: Phase::Off }
				]
			);
			assert_eq!(verifier_events(), vec![]);

			// This will set us to emergency phase, because we don't know wtf to do.
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
		});
	}

	#[test]
	fn multi_page_fallback_shortcut_to_msp_works() {
		ExtBuilder::full().fallback_mode(FallbackModes::Onchain).build_and_execute(|| {
			roll_to_signed_open();

			// but then we immediately call `elect`, this will work
			assert!(MultiBlock::elect(0).is_ok());

			assert_eq!(
				multi_block_events(),
				vec![
					Event::PhaseTransitioned {
						from: Phase::Off,
						to: Phase::Snapshot(Pages::get())
					},
					Event::PhaseTransitioned {
						from: Phase::Snapshot(0),
						to: Phase::Signed(SignedPhase::get() - 1)
					},
					Event::PhaseTransitioned {
						from: Phase::Signed(SignedPhase::get() - 1),
						to: Phase::Off
					}
				]
			);

			// This will set us to the off phase, since fallback saved us.
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
		});
	}

	#[test]
	fn elect_call_when_not_ongoing() {
		ExtBuilder::full().fallback_mode(FallbackModes::Onchain).build_and_execute(|| {
			roll_to_snapshot_created();
			assert_eq!(MultiBlock::status(), Ok(None));
			assert!(MultiBlock::elect(0).is_ok());
		});
		ExtBuilder::full().fallback_mode(FallbackModes::Onchain).build_and_execute(|| {
			roll_to(10);
			assert_eq!(MultiBlock::status(), Err(()));
			assert_eq!(MultiBlock::elect(0), Err(ElectionError::NotOngoing));
		});
	}
}

#[cfg(test)]
mod manage_ops {
	use super::*;
	use crate::mock::*;

	#[test]
	fn trigger_fallback_works() {
		ExtBuilder::full()
			.fallback_mode(FallbackModes::Emergency)
			.build_and_execute(|| {
				roll_to_signed_open();

				// bad origin cannot call
				assert_noop!(
					MultiBlock::manage(
						RuntimeOrigin::signed(Manager::get() + 1),
						ManagerOperation::EmergencyFallback
					),
					DispatchError::BadOrigin
				);

				// we get a call to elect(0). this will cause emergency, since no fallback is
				// allowed.
				assert_eq!(
					MultiBlock::elect(0),
					Err(ElectionError::Fallback("Emergency phase started.".to_string()))
				);
				assert_eq!(MultiBlock::current_phase(), Phase::Emergency);

				// we can now set the solution to emergency, assuming fallback is set to onchain
				FallbackMode::set(FallbackModes::Onchain);
				assert_ok!(MultiBlock::manage(
					RuntimeOrigin::signed(Manager::get()),
					ManagerOperation::EmergencyFallback
				));

				assert_eq!(MultiBlock::current_phase(), Phase::Emergency);
				assert_ok!(MultiBlock::elect(0));
				assert_eq!(MultiBlock::current_phase(), Phase::Off);

				assert_eq!(
					multi_block_events(),
					vec![
						Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
						Event::PhaseTransitioned {
							from: Phase::Snapshot(0),
							to: Phase::Signed(SignedPhase::get() - 1)
						},
						Event::PhaseTransitioned {
							from: Phase::Signed(SignedPhase::get() - 1),
							to: Phase::Emergency
						},
						Event::PhaseTransitioned { from: Phase::Emergency, to: Phase::Off }
					]
				);
				assert_eq!(
					verifier_events(),
					vec![verifier::Event::Queued(
						ElectionScore { minimal_stake: 15, sum_stake: 40, sum_stake_squared: 850 },
						None
					)]
				);
			})
	}

	// This scenario have multiple outcomes:
	// 1. rotate in off => almost a noop
	// 2. rotate mid signed, validation, unsigned, done, but NOT export => clear all data, move to
	//    next round and be off. Note: all of the data in this pallet is indexed by the round index,
	//    so moving to the next round will implicitly make the old data unavaioable, even if not
	//    cleared out. This secnario needs further testing.
	// 3. rotate mid export: same as above, except staking will be out of sync and will also need
	//    governance intervention.
	//
	// This test is primarily checking the origin limits of this call, and will use scenario 2.
	#[test]
	fn force_rotate_round() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			let round = MultiBlock::round();
			let paged =
				unsigned::miner::OffchainWorkerMiner::<Runtime>::mine_solution(Pages::get(), false)
					.unwrap();
			load_signed_for_verification_and_start_and_roll_to_verified(99, paged, 0);

			// we have snapshot data now for this round.
			assert_full_snapshot();
			// there is some data in the verifier pallet
			assert!(verifier::QueuedSolution::<T>::queued_score().is_some());
			// phase is
			assert_eq!(MultiBlock::current_phase(), Phase::SignedValidation(2));

			// force new round

			// bad origin cannot submit
			assert_noop!(
				MultiBlock::manage(
					RuntimeOrigin::signed(Manager::get() + 1),
					ManagerOperation::ForceRotateRound
				),
				DispatchError::BadOrigin
			);
			// manager can submit
			assert_ok!(MultiBlock::manage(
				RuntimeOrigin::signed(Manager::get()),
				ManagerOperation::ForceRotateRound
			));

			// phase is off again
			assert_eq!(MultiBlock::current_phase(), Phase::Off);
			// round is bumped
			assert_eq!(MultiBlock::round(), round + 1);
			// snapshot is wiped
			assert_none_snapshot();
			// verifier is in clean state
			verifier::QueuedSolution::<T>::assert_killed();
		});
	}

	#[test]
	fn force_set_phase() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();
			assert_eq!(MultiBlock::current_phase(), Phase::Signed(SignedPhase::get() - 1));

			// bad origin cannot submit
			assert_noop!(
				MultiBlock::manage(
					RuntimeOrigin::signed(Manager::get() + 1),
					ManagerOperation::ForceSetPhase(Phase::Done)
				),
				DispatchError::BadOrigin
			);

			// manager can submit. They skip the signed phases.
			assert_ok!(MultiBlock::manage(
				RuntimeOrigin::signed(Manager::get()),
				ManagerOperation::ForceSetPhase(Phase::Unsigned(UnsignedPhase::get() - 1))
			));

			assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(UnsignedPhase::get() - 1));

			// admin can also submit
			assert_ok!(MultiBlock::manage(
				RuntimeOrigin::root(),
				ManagerOperation::ForceSetPhase(Phase::Unsigned(UnsignedPhase::get() - 2))
			));
			assert_eq!(MultiBlock::current_phase(), Phase::Unsigned(UnsignedPhase::get() - 2));
		});
	}
}
#[cfg(test)]
mod admin_ops {
	use super::*;
	use crate::mock::*;

	#[test]
	fn set_solution_emergency_works() {
		ExtBuilder::full().build_and_execute(|| {
			roll_to_signed_open();

			// bad origin cannot call
			assert_noop!(
				MultiBlock::admin(
					RuntimeOrigin::signed(Manager::get() + 1),
					AdminOperation::EmergencySetSolution(Default::default(), Default::default())
				),
				DispatchError::BadOrigin
			);

			// manager (non-root) cannot call
			assert_noop!(
				MultiBlock::admin(
					RuntimeOrigin::signed(Manager::get()),
					AdminOperation::EmergencySetSolution(Default::default(), Default::default())
				),
				DispatchError::BadOrigin
			);

			// we get a call to elect(0). this will cause emergency, since no fallback is allowed.
			assert_eq!(
				MultiBlock::elect(0),
				Err(ElectionError::Fallback("Emergency phase started.".to_string()))
			);
			assert_eq!(MultiBlock::current_phase(), Phase::Emergency);

			// we can now set the solution to emergency.
			let (emergency, score) = emergency_solution();
			assert_ok!(MultiBlock::admin(
				RuntimeOrigin::root(),
				AdminOperation::EmergencySetSolution(Box::new(emergency), score)
			));

			assert_eq!(MultiBlock::current_phase(), Phase::Emergency);
			assert_ok!(MultiBlock::elect(0));
			assert_eq!(MultiBlock::current_phase(), Phase::Off);

			assert_eq!(
				multi_block_events(),
				vec![
					Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
					Event::PhaseTransitioned {
						from: Phase::Snapshot(0),
						to: Phase::Signed(SignedPhase::get() - 1)
					},
					Event::PhaseTransitioned {
						from: Phase::Signed(SignedPhase::get() - 1),
						to: Phase::Emergency
					},
					Event::PhaseTransitioned { from: Phase::Emergency, to: Phase::Off }
				]
			);
			assert_eq!(
				verifier_events(),
				vec![verifier::Event::Queued(
					ElectionScore { minimal_stake: 55, sum_stake: 130, sum_stake_squared: 8650 },
					None
				)]
			);
		})
	}

	#[test]
	fn set_minimum_solution_score() {
		ExtBuilder::full().build_and_execute(|| {
			// bad origin cannot call
			assert_noop!(
				MultiBlock::admin(
					RuntimeOrigin::signed(Manager::get() + 1),
					AdminOperation::SetMinUntrustedScore(ElectionScore {
						minimal_stake: 100,
						..Default::default()
					})
				),
				DispatchError::BadOrigin
			);

			// manager cannot call, only admin.
			assert_noop!(
				MultiBlock::admin(
					RuntimeOrigin::signed(Manager::get()),
					AdminOperation::SetMinUntrustedScore(ElectionScore {
						minimal_stake: 100,
						..Default::default()
					})
				),
				DispatchError::BadOrigin
			);

			assert_eq!(VerifierPallet::minimum_score(), None);
			assert_ok!(MultiBlock::admin(
				RuntimeOrigin::root(),
				AdminOperation::SetMinUntrustedScore(ElectionScore {
					minimal_stake: 100,
					..Default::default()
				})
			));
			assert_eq!(
				VerifierPallet::minimum_score().unwrap(),
				ElectionScore { minimal_stake: 100, ..Default::default() }
			);
		});
	}
}
