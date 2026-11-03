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

use super::{AccountId, Balance, Balances, Pages, Runtime, RuntimeEvent, SignedPallet, System};
use crate::{
	mock::*,
	signed::{self as signed_pallet, ActivePot, Event as SignedEvent, Submissions},
	unsigned::miner::MinerConfig,
	verifier::{self, AsynchronousVerifier, SolutionDataProvider, VerificationResult, Verifier},
	Event, PadSolutionPages, PagedRawSolution, Pagify, Phase, SolutionOf,
};
use frame_election_provider_support::PageIndex;
use frame_support::{
	assert_ok,
	dispatch::{DispatchInfo, PostDispatchInfo},
	parameter_types,
	traits::{
		fungible::{Balanced, Credit},
		EstimateCallFee, EstimateFee, OnUnbalanced,
	},
};
use sp_npos_elections::ElectionScore;
use sp_runtime::{traits::Zero, Perbill};

parameter_types! {
	pub static MockSignedNextSolution: Option<Vec<SolutionOf<Runtime>>> = None;
	pub static MockSignedNextScore: ElectionScore = Default::default();
	pub static MockSignedResults: Vec<VerificationResult> = Default::default();
}

/// A simple implementation of the signed phase that can be controller by some static variables
/// directly.
///
/// Useful for when you don't care too much about the signed phase.
#[allow(dead_code)]
pub struct MockSignedPhase;
impl SolutionDataProvider for MockSignedPhase {
	type Solution = <Runtime as MinerConfig>::Solution;
	fn get_page(page: PageIndex) -> Self::Solution {
		MockSignedNextSolution::get()
			.and_then(|i| i.get(page as usize).cloned())
			.unwrap_or_default()
	}

	fn get_score() -> ElectionScore {
		MockSignedNextScore::get()
	}

	fn report_result(result: verifier::VerificationResult) {
		MOCK_SIGNED_RESULTS.with(|r| r.borrow_mut().push(result));
	}
}

pub struct FixedCallFee;
impl EstimateCallFee<signed_pallet::Call<Runtime>, Balance> for FixedCallFee {
	fn estimate_call_fee(_: &signed_pallet::Call<Runtime>, _: PostDispatchInfo) -> Balance {
		1
	}
}
impl EstimateFee<Balance> for FixedCallFee {
	fn estimate_fee(_: u32, _: &DispatchInfo) -> Balance {
		1
	}
}

parameter_types! {
	pub static SignedDepositBase: Balance = 5;
	pub static SignedDepositPerPage: Balance = 1;
	pub static InvulnerableDeposit: Balance = 7;
	pub static SignedMaxSubmissions: u32 = 3;
	pub static SignedRewardBase: Balance = 3;
	pub static SignedPhaseSwitch: SignedSwitch = SignedSwitch::Real;
	pub static BailoutGraceRatio: Perbill = Perbill::from_percent(20);
	pub static EjectGraceRatio: Perbill = Perbill::from_percent(20);
	pub static SignedRewardSource: Option<AccountId> = None;
	pub static SignedSlashTarget: Option<AccountId> = None;
}

/// Routes slashed deposits to [`SignedSlashTarget`] if set, otherwise drops (burns) them.
pub struct MockSlash;
impl OnUnbalanced<Credit<AccountId, Balances>> for MockSlash {
	fn on_unbalanced(credit: Credit<AccountId, Balances>) {
		if let Some(target) = SignedSlashTarget::get() {
			let _ = Balances::resolve(&target, credit).map_err(|c| drop(c));
		}
		// else: `credit` is dropped here.
	}
}

impl crate::signed::Config for Runtime {
	type Currency = Balances;
	type DepositBase = SignedDepositBase;
	type DepositPerPage = SignedDepositPerPage;
	type InvulnerableDeposit = InvulnerableDeposit;
	type EstimateCallFee = FixedCallFee;
	type MaxFeeRefund = crate::signed::FullSubmissionFee<Runtime, FixedCallFee>;
	type MaxSubmissions = SignedMaxSubmissions;
	type RewardBase = SignedRewardBase;
	type BailoutGraceRatio = BailoutGraceRatio;
	type EjectGraceRatio = EjectGraceRatio;
	type Slash = MockSlash;
	type RewardSource = ActivePot<SignedRewardSource>;
	type WeightInfo = ();
}

/// Control which signed phase is being used.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SignedSwitch {
	Mock,
	Real,
}

pub struct DualSignedPhase;
impl SolutionDataProvider for DualSignedPhase {
	type Solution = <Runtime as MinerConfig>::Solution;
	fn get_page(page: PageIndex) -> Self::Solution {
		match SignedPhaseSwitch::get() {
			SignedSwitch::Mock => MockSignedNextSolution::get()
				.and_then(|i| i.get(page as usize).cloned())
				.unwrap_or_default(),
			SignedSwitch::Real => SignedPallet::get_page(page),
		}
	}

	fn get_score() -> ElectionScore {
		match SignedPhaseSwitch::get() {
			SignedSwitch::Mock => MockSignedNextScore::get(),
			SignedSwitch::Real => SignedPallet::get_score(),
		}
	}

	fn report_result(result: verifier::VerificationResult) {
		match SignedPhaseSwitch::get() {
			SignedSwitch::Mock => MOCK_SIGNED_RESULTS.with(|r| r.borrow_mut().push(result)),
			SignedSwitch::Real => SignedPallet::report_result(result),
		}
	}
}

parameter_types! {
	static SignedEventsIndex: u32 = 0;
}

pub fn signed_events_since_last_call() -> Vec<crate::signed::Event<Runtime>> {
	let events = signed_events();
	let already_seen = SignedEventsIndex::get();
	SignedEventsIndex::set(events.len() as u32);
	events.into_iter().skip(already_seen as usize).collect()
}

/// get the events of the verifier pallet.
pub fn signed_events() -> Vec<crate::signed::Event<Runtime>> {
	System::events()
		.into_iter()
		.map(|r| r.event)
		.filter_map(|e| if let RuntimeEvent::SignedPallet(inner) = e { Some(inner) } else { None })
		.collect::<Vec<_>>()
}

/// Load a signed solution into its pallet.
pub fn load_signed_for_verification(who: AccountId, paged: PagedRawSolution<Runtime>) {
	let initial_balance = Balances::free_balance(&who);
	assert_eq!(balances(who), (initial_balance, 0));

	assert_ok!(SignedPallet::register(RuntimeOrigin::signed(who), paged.score));

	assert_eq!(
		balances(who),
		(initial_balance - SignedDepositBase::get(), SignedDepositBase::get())
	);

	for (page_index, solution_page) in paged.solution_pages.pagify(Pages::get()) {
		assert_ok!(SignedPallet::submit_page(
			RuntimeOrigin::signed(who),
			page_index,
			Some(Box::new(solution_page.clone()))
		));
	}

	let mut events = signed_events();
	for _ in 0..Pages::get() {
		let event = events.pop().unwrap();
		assert!(matches!(event, SignedEvent::Stored(_, x, _) if x == who))
	}
	assert!(matches!(events.pop().unwrap(), SignedEvent::Registered(_, x, _) if x == who));

	let full_deposit =
		SignedDepositBase::get() + (Pages::get() as Balance) * SignedDepositPerPage::get();
	assert_eq!(balances(who), (initial_balance - full_deposit, full_deposit));
}

/// Same as [`load_signed_for_verification`], but also goes forward to the beginning of the signed
/// verification phase.
pub fn load_signed_for_verification_and_start(
	who: AccountId,
	paged: PagedRawSolution<Runtime>,
	_round: u32,
) {
	load_signed_for_verification(who, paged);

	// now the solution should start being verified.
	roll_to_signed_validation_open();
	assert_eq!(
		multi_block_events(),
		vec![
			Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) },
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
	assert_eq!(verifier_events(), vec![]);
}

/// Same as [`load_signed_for_verification_and_start`], but also goes forward enough blocks for the
/// solution to be verified, assuming it is all correct.
///
/// In other words, it goes [`Pages`] blocks forward.
pub fn load_signed_for_verification_and_start_and_roll_to_verified(
	who: AccountId,
	paged: PagedRawSolution<Runtime>,
	_round: u32,
) {
	load_signed_for_verification(who, paged.clone());

	// now the solution should start being verified.
	roll_to_signed_validation_open();
	assert_eq!(
		multi_block_events(),
		vec![
			Event::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(Pages::get()) },
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
	assert_eq!(verifier_events(), vec![]);

	// there is no queued solution prior to the last page of the solution getting verified
	assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), None);

	// roll to the block it is finalized.
	for _ in 0..Pages::get() + 1 {
		roll_next();
	}

	assert_eq!(
		verifier_events(),
		vec![
			// NOTE: these are hardcoded for 3 page.
			verifier::Event::Verified(2, 2),
			verifier::Event::Verified(1, 2),
			verifier::Event::Verified(0, 2),
			verifier::Event::Queued(paged.score, None),
		]
	);

	// there is now a queued solution.
	assert_eq!(<Runtime as crate::Config>::Verifier::queued_score(), Some(paged.score));
}

/// Load a full raw paged solution for verification.
///
/// More or less the equivalent of `load_signed_for_verification_and_start`, but when
/// `SignedSwitch::Mock` is set.
pub fn load_mock_signed_and_start(raw_paged: PagedRawSolution<Runtime>) {
	assert_eq!(
		SignedPhaseSwitch::get(),
		SignedSwitch::Mock,
		"you should not use this if mock phase is not being mocked"
	);
	MockSignedNextSolution::set(Some(raw_paged.solution_pages.pad_solution_pages(Pages::get())));
	MockSignedNextScore::set(raw_paged.score);

	// Let's gooooo!
	assert_ok!(<VerifierPallet as AsynchronousVerifier>::start());
}

/// Ensure that no submission data exists in `round` for `who`.
pub fn assert_no_data_for(round: u32, who: AccountId) {
	assert!(!Submissions::<Runtime>::leaderboard(round).into_iter().any(|(x, _)| x == who));
	assert!(Submissions::<Runtime>::metadata_of(round, who).is_none());
	assert!(Submissions::<Runtime>::pages_of(round, who).count().is_zero());
}
