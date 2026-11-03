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

use super::{Event as SignedEvent, *};
use crate::{
	mock::*,
	types::Pagify,
	verifier::{FeasibilityError, Verifier},
	Phase,
};
use frame_election_provider_support::{ElectionProvider, NposSolution};
use frame_support::storage::unhashed;
use sp_core::bounded_vec;
use sp_npos_elections::ElectionScore;

pub type T = Runtime;

fn score_from(x: u128) -> ElectionScore {
	ElectionScore { minimal_stake: x, ..Default::default() }
}

fn max_fee_refund() -> Balance {
	<Runtime as crate::signed::Config>::MaxFeeRefund::get()
}

mod calls {
	use super::*;
	use sp_runtime::{DispatchError, TokenError::FundsUnavailable};

	#[test]
	fn cannot_register_with_insufficient_balance() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			// 777 is not funded.
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(777), Default::default()),
				DispatchError::Token(FundsUnavailable)
			);
		});

		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			// 99 is funded but deposit is too high.
			assert_eq!(balances(99), (100, 0));
			SignedDepositBase::set(101);
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(99), Default::default()),
				DispatchError::Token(FundsUnavailable)
			);
		})
	}

	#[test]
	fn cannot_register_if_not_signed() {
		ExtBuilder::signed().build_and_execute(|| {
			assert!(!crate::Pallet::<T>::current_phase().is_signed());
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(99), Default::default()),
				Error::<T>::PhaseNotSigned
			);
		})
	}

	#[test]
	fn register_metadata_works() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			assert_eq!(balances(99), (100, 0));
			let score = ElectionScore { minimal_stake: 100, ..Default::default() };

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_eq!(balances(99), (95, 5));

			assert_eq!(Submissions::<Runtime>::metadata_iter(1).count(), 0);
			assert_eq!(Submissions::<Runtime>::metadata_iter(0).count(), 1);
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 5,
					fee: 1,
					pages: bounded_vec![false, false, false],
					reward: 3
				}
			);
			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![(99, ElectionScore { minimal_stake: 100, ..Default::default() })]
			);
			assert!(matches!(signed_events().as_slice(), &[
					SignedEvent::Registered(_, x, _),
				] if x == 99));

			// second ones submits
			assert_eq!(balances(999), (100, 0));
			let score = ElectionScore { minimal_stake: 90, ..Default::default() };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(999), score));
			assert_eq!(balances(999), (95, 5));

			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 999).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 5,
					fee: 1,
					pages: bounded_vec![false, false, false],
					reward: 3
				}
			);
			assert!(matches!(signed_events().as_slice(), &[
					SignedEvent::Registered(..),
					SignedEvent::Registered(_, x, _),
				] if x == 999));

			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![
					(999, ElectionScore { minimal_stake: 90, ..Default::default() }),
					(99, ElectionScore { minimal_stake: 100, ..Default::default() })
				]
			);
			assert_eq!(Submissions::<Runtime>::metadata_iter(1).count(), 0);
			assert_eq!(Submissions::<Runtime>::metadata_iter(0).count(), 2);

			// submit again with a new score.
			assert_noop!(
				SignedPallet::register(
					RuntimeOrigin::signed(999),
					ElectionScore { minimal_stake: 80, ..Default::default() }
				),
				Error::<T>::Duplicate,
			);
		})
	}

	#[test]
	fn page_submission_accumulates_fee() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			let score = ElectionScore { minimal_stake: 100, ..Default::default() };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));

			// fee for register is recorded.
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 5,
					fee: 1,
					pages: bounded_vec![false, false, false],
					reward: 3
				}
			);

			// fee for page submission is recorded.
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				0,
				Some(Default::default())
			));
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 6,
					fee: 2,
					pages: bounded_vec![true, false, false],
					reward: 3
				}
			);

			// another fee for page submission is recorded.
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				1,
				Some(Default::default())
			));
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 7,
					fee: 3,
					pages: bounded_vec![true, true, false],
					reward: 3
				}
			);

			// removal updates deposit but not the fee
			assert_ok!(SignedPallet::submit_page(RuntimeOrigin::signed(99), 1, None));

			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 6,
					fee: 3,
					pages: bounded_vec![true, false, false],
					reward: 3
				}
			);
		});
	}

	#[test]
	fn resubmitting_a_stored_page_does_not_accumulate_fee() {
		ExtBuilder::signed().build_and_execute(|| {
			// GIVEN a registered submitter with one page stored.
			roll_to_signed_open();
			assert_full_snapshot();

			let score = ElectionScore { minimal_stake: 100, ..Default::default() };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				0,
				Some(Default::default())
			));

			let before = Submissions::<Runtime>::metadata_of(0, 99).unwrap();
			assert_eq!(before.fee, 2);

			// WHEN the same page is stored again.
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				0,
				Some(Default::default())
			));

			// THEN nothing in the metadata moves: no second fee for the same page, and the page
			// count, hence the deposit, is the same.
			assert_eq!(Submissions::<Runtime>::metadata_of(0, 99).unwrap(), before);
		});
	}

	#[test]
	fn metadata_submission_sorted_based_on_score() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(91), score_from(100)));
			assert_eq!(*Submissions::<Runtime>::leaderboard(0), vec![(91, score_from(100))]);
			assert_eq!(balances(91), (95, 5));
			assert!(matches!(signed_events().as_slice(), &[SignedEvent::Registered(_, 91, _)]));

			// weaker one comes while we have space.
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(92), score_from(90)));
			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![(92, score_from(90)), (91, score_from(100))]
			);
			assert_eq!(balances(92), (95, 5));
			assert!(matches!(
				signed_events().as_slice(),
				&[SignedEvent::Registered(..), SignedEvent::Registered(_, 92, _),]
			));

			// stronger one comes while we have have space.
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(93), score_from(110)));
			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![(92, score_from(90)), (91, score_from(100)), (93, score_from(110))]
			);
			assert_eq!(balances(93), (95, 5));
			assert!(matches!(
				signed_events().as_slice(),
				&[
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
					SignedEvent::Registered(_, 93, _),
				]
			));

			// weaker one comes while we don't have space.
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(94), score_from(80)),
				Error::<T>::QueueFull
			);
			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![(92, score_from(90)), (91, score_from(100)), (93, score_from(110))]
			);
			assert_eq!(balances(94), (100, 0));
			// no event has been emitted this time.
			assert!(matches!(
				signed_events().as_slice(),
				&[
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
				]
			));

			// stronger one comes while we don't have space. Eject the weakest
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(94), score_from(120)));
			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![(91, score_from(100)), (93, score_from(110)), (94, score_from(120))]
			);
			assert!(matches!(
				signed_events().as_slice(),
				&[
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
					SignedEvent::Slashed(_, 92, _),
					SignedEvent::Ejected(_, 92),
					SignedEvent::Registered(_, 94, _),
				]
			));
			assert_eq!(balances(94), (95, 5));
			// 92 is ejected, 1 unit of deposit is refunded, 4 units are slashed.
			// see the default `EjectGraceRatio`.
			assert_eq!(balances(92), (96, 0));

			// another stronger one comes, only replace the weakest.
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(95), score_from(105)));
			assert_eq!(
				*Submissions::<Runtime>::leaderboard(0),
				vec![(95, score_from(105)), (93, score_from(110)), (94, score_from(120))]
			);
			assert_eq!(balances(95), (95, 5));
			// 91 is ejected, they get only a part of the deposit back.
			assert_eq!(balances(91), (96, 0));
			assert!(matches!(
				signed_events().as_slice(),
				&[
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
					SignedEvent::Registered(..),
					SignedEvent::Slashed(..),
					SignedEvent::Ejected(..),
					SignedEvent::Registered(..),
					SignedEvent::Slashed(_, 91, _),
					SignedEvent::Ejected(_, 91),
					SignedEvent::Registered(_, 95, _),
				]
			));
		})
	}

	#[test]
	fn can_bail_at_a_cost() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			let score = ElectionScore { minimal_stake: 100, ..Default::default() };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_eq!(balances(99), (95, 5));

			// not submitted, cannot bailout.
			assert_noop!(SignedPallet::bail(RuntimeOrigin::signed(999)), Error::<T>::NoSubmission);

			// can bail.
			assert_ok!(SignedPallet::bail(RuntimeOrigin::signed(99)));
			// 20% of the deposit returned, which is 1, 4 is slashed.
			assert_eq!(balances(99), (96, 0));
			assert_no_data_for(0, 99);

			assert_eq!(
				signed_events(),
				vec![
					Event::Registered(0, 99, score),
					Event::Slashed(0, 99, 4),
					Event::Bailed(0, 99)
				]
			);
		});
	}

	#[test]
	fn can_submit_pages() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			assert_noop!(
				SignedPallet::submit_page(RuntimeOrigin::signed(99), 0, Default::default()),
				Error::<T>::NotRegistered
			);

			assert_ok!(SignedPallet::register(
				RuntimeOrigin::signed(99),
				ElectionScore { minimal_stake: 100, ..Default::default() }
			));

			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), 0);
			assert_eq!(balances(99), (95, 5));

			// indices 0, 1, 2 are valid.
			assert_noop!(
				SignedPallet::submit_page(RuntimeOrigin::signed(99), 3, Default::default()),
				Error::<T>::BadPageIndex
			);

			// add the first page.
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				0,
				Some(Default::default())
			));
			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), 1);
			assert_eq!(balances(99), (94, 6));
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap().pages.into_inner(),
				vec![true, false, false]
			);

			// replace it again, nada.
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				0,
				Some(Default::default())
			));
			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), 1);
			assert_eq!(balances(99), (94, 6));

			// add a new one.
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				1,
				Some(Default::default())
			));
			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), 2);
			assert_eq!(balances(99), (93, 7));
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap().pages.into_inner(),
				vec![true, true, false]
			);

			// remove one, deposit is back.
			assert_ok!(SignedPallet::submit_page(RuntimeOrigin::signed(99), 0, None));
			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), 1);
			assert_eq!(balances(99), (94, 6));
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap().pages.into_inner(),
				vec![false, true, false]
			);

			assert!(matches!(
				signed_events().as_slice(),
				&[
					SignedEvent::Registered(..),
					SignedEvent::Stored(.., 0),
					SignedEvent::Stored(.., 0),
					SignedEvent::Stored(.., 1),
					SignedEvent::Stored(.., 0),
				]
			));
		});
	}
}

mod max_fee_refund {
	use super::*;
	use codec::Encode;
	use frame_support::{dispatch::DispatchInfo, traits::EstimateFee};

	/// A fee model that charges one unit per encoded byte, and nothing for weight.
	pub struct PerByteFee;
	impl EstimateFee<Balance> for PerByteFee {
		fn estimate_fee(len: u32, _: &DispatchInfo) -> Balance {
			len.into()
		}
	}

	#[test]
	fn is_what_a_full_submission_accrues() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			// GIVEN a full submission, under a fee model that charges one unit per call.
			load_signed_for_verification(99, mine_full_solution().unwrap());
			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), Pages::get() as usize);

			// THEN the bound is exactly what it accrued: one `register`, plus one call per page.
			let accrued = Submissions::<Runtime>::metadata_of(0, 99).unwrap().fee;
			assert_eq!(accrued, 1 + Pages::get() as Balance);
			assert_eq!(accrued, max_fee_refund());
		})
	}

	#[test]
	fn covers_the_length_of_a_real_submission() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			// GIVEN the calls that a full submission is made of, priced by encoded length.
			let paged = mine_full_solution().unwrap();
			let mut submission_len =
				crate::signed::Call::<Runtime>::register { claimed_score: paged.score }
					.encoded_size();
			for (page, solution) in paged.solution_pages.pagify(Pages::get()) {
				submission_len += crate::signed::Call::<Runtime>::submit_page {
					page,
					maybe_solution: Some(Box::new(solution.clone())),
				}
				.encoded_size();
			}

			// THEN the bound covers it with room to spare: pages are priced at the maximum page
			// size, not their actual size.
			assert!(FullSubmissionFee::<Runtime, PerByteFee>::get() > submission_len as Balance);
		})
	}
}

mod e2e {
	use super::*;
	#[test]
	fn good_bad_evil() {
		// an extensive scenario: 3 solutions submitted, once rewarded, one slashed, and one
		// discarded.
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			// an invalid, but weak solution.
			{
				let score =
					ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 };
				assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
				assert_ok!(SignedPallet::submit_page(
					RuntimeOrigin::signed(99),
					0,
					Some(Default::default())
				));

				assert_eq!(balances(99), (94, 6));
			}

			// a valid, strong solution.
			let strong_score = {
				let paged = mine_full_solution().unwrap();
				load_signed_for_verification(999, paged.clone());
				assert_eq!(balances(999), (92, 8));
				paged.score
			};

			// an invalid, strong solution.
			{
				let mut score = strong_score;
				score.minimal_stake *= 2;
				assert_ok!(SignedPallet::register(RuntimeOrigin::signed(92), score));
				assert_eq!(balances(92), (95, 5));
				// we don't even bother to submit a page..
			}

			assert_eq!(
				Submissions::<Runtime>::leaderboard(0)
					.into_iter()
					.map(|(x, _)| x)
					.collect::<Vec<_>>(),
				vec![99, 999, 92]
			);

			assert_eq!(
				Submissions::<Runtime>::metadata_iter(0).collect::<Vec<_>>(),
				vec![
					(
						92,
						SubmissionMetadata {
							deposit: 5,
							fee: 1,
							reward: 3,
							claimed_score: ElectionScore {
								minimal_stake: 110,
								sum_stake: 130,
								sum_stake_squared: 8650
							},
							pages: bounded_vec![false, false, false]
						}
					),
					(
						999,
						SubmissionMetadata {
							deposit: 8,
							fee: 4,
							reward: 3,
							claimed_score: ElectionScore {
								minimal_stake: 55,
								sum_stake: 130,
								sum_stake_squared: 8650
							},
							pages: bounded_vec![true, true, true]
						}
					),
					(
						99,
						SubmissionMetadata {
							deposit: 6,
							fee: 2,
							reward: 3,
							claimed_score: ElectionScore {
								minimal_stake: 10,
								sum_stake: 10,
								sum_stake_squared: 100
							},
							pages: bounded_vec![true, false, false]
						}
					)
				]
			);
			assert_eq!(
				signed_events_since_last_call(),
				vec![
					SignedEvent::Registered(
						0,
						99,
						ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 }
					),
					SignedEvent::Stored(0, 99, 0),
					SignedEvent::Registered(
						0,
						999,
						ElectionScore {
							minimal_stake: 55,
							sum_stake: 130,
							sum_stake_squared: 8650
						}
					),
					SignedEvent::Stored(0, 999, 0),
					SignedEvent::Stored(0, 999, 1),
					SignedEvent::Stored(0, 999, 2),
					SignedEvent::Registered(
						0,
						92,
						ElectionScore {
							minimal_stake: 110,
							sum_stake: 130,
							sum_stake_squared: 8650
						}
					)
				]
			);

			roll_to_signed_validation_open();

			// 92 is slashed in 3 blocks, 999 becomes rewarded in 3 blocks, and 99 is discarded.
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(5), Status::Ongoing(1));
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(4), Status::Ongoing(0));
			// Note `Ongoing(2)` -- verifier is signaled to continue.
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(3), Status::Ongoing(2));

			// Check events after first solution (92) is rejected
			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					crate::verifier::Event::Verified(2, 0),
					crate::verifier::Event::Verified(1, 0),
					crate::verifier::Event::Verified(0, 0),
					crate::verifier::Event::VerificationFailed(0, FeasibilityError::InvalidScore),
				]
			);
			// and that submitter is slashed.
			assert_eq!(signed_events_since_last_call(), vec![SignedEvent::Slashed(0, 92, 5)]);

			assert_eq!(
				Submissions::<Runtime>::leaderboard(0)
					.into_iter()
					.map(|(x, _)| x)
					.collect::<Vec<_>>(),
				vec![99, 999]
			);

			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(2), Status::Ongoing(1));
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(1), Status::Ongoing(0));
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(0), Status::Nothing);

			// Check events after second solution (999) is accepted
			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					crate::verifier::Event::Verified(2, 2),
					crate::verifier::Event::Verified(1, 2),
					crate::verifier::Event::Verified(0, 2),
					crate::verifier::Event::Queued(
						ElectionScore {
							minimal_stake: 55,
							sum_stake: 130,
							sum_stake_squared: 8650
						},
						None
					)
				]
			);

			assert_eq!(signed_events_since_last_call(), vec![SignedEvent::Rewarded(0, 999, 7)]);

			// 99 is discarded -- for now they have some deposit collected, which they have to
			// manually collect next.
			assert_eq!(balances(99), (94, 6));
			// 999 has gotten their deposit back, plus fee and reward back.
			assert_eq!(balances(999), (107, 0));
			// 92 loses a part of their deposit for being ejected.
			assert_eq!(balances(92), (95, 0));

			// the data associated with 999 is already removed.
			assert_ok!(Submissions::<Runtime>::ensure_killed_with(&999, 0));
			// the data associated with 92 is already removed.
			assert_ok!(Submissions::<Runtime>::ensure_killed_with(&92, 0));
			// but not for 99
			assert!(Submissions::<Runtime>::ensure_killed_with(&99, 0).is_err());

			// we cannot cleanup just yet.
			assert_noop!(
				SignedPallet::clear_old_round_data(RuntimeOrigin::signed(99), 0, Pages::get()),
				Error::<T>::RoundNotOver
			);

			MultiBlock::rotate_round();

			// now we can delete our stuff.
			assert_ok!(SignedPallet::clear_old_round_data(
				RuntimeOrigin::signed(99),
				0,
				Pages::get()
			));
			// our stuff is gone.
			assert_ok!(Submissions::<Runtime>::ensure_killed_with(&99, 0));

			// check events.
			assert_eq!(signed_events_since_last_call(), vec![Event::Discarded(0, 99)]);

			// 99 now has their deposit returned.
			assert_eq!(balances(99), (100, 0));

			// signed pallet should be in 100% clean state.
			assert_ok!(Submissions::<Runtime>::ensure_killed(0));
		})
	}

	#[test]
	fn winner_fee_refund_is_bounded_by_max_fee_refund() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			// GIVEN a full, valid solution, whose accrued fee is already at the bound.
			let paged = mine_full_solution().unwrap();
			load_signed_for_verification(99, paged.clone());
			assert_eq!(Submissions::<Runtime>::metadata_of(0, 99).unwrap().fee, max_fee_refund());

			// WHEN further fee-paying calls are made against the same submission, leaving it
			// unchanged.
			let (page_index, page) = paged.solution_pages.pagify(Pages::get()).next().unwrap();
			for _ in 0..3 {
				assert_ok!(SignedPallet::submit_page(RuntimeOrigin::signed(99), page_index, None));
				assert_ok!(SignedPallet::submit_page(
					RuntimeOrigin::signed(99),
					page_index,
					Some(Box::new(page.clone()))
				));
			}
			assert_eq!(Submissions::<Runtime>::pages_of(0, 99).count(), Pages::get() as usize);
			assert_eq!(Submissions::<Runtime>::get_page_of(0, &99, page_index), Some(page.clone()));
			assert!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap().fee > max_fee_refund(),
				"the bound must actually bind for this test to mean anything"
			);
			let _ = signed_events_since_last_call();

			// THEN the winner is paid the reward plus the bound, not the accrued fee.
			roll_to_signed_validation_open();
			roll_to_full_verification();

			assert_eq!(
				signed_events_since_last_call(),
				vec![SignedEvent::Rewarded(0, 99, SignedRewardBase::get() + max_fee_refund())]
			);
			// deposit released, and only the bounded refund minted on top of the reward.
			assert_eq!(balances(99), (100 + SignedRewardBase::get() + max_fee_refund(), 0));
		})
	}

	#[test]
	fn after_rejecting_does_not_call_verifier_start_if_no_leader_exists() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			// Submit only an invalid solution (register but don't submit pages)
			let invalid_score =
				ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), invalid_score));

			let current_round = SignedPallet::current_round();
			assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 1);

			roll_to_signed_validation_open();
			roll_to_full_verification();

			// Check that rejection events were properly generated
			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					crate::verifier::Event::Verified(2, 0),
					crate::verifier::Event::Verified(1, 0),
					crate::verifier::Event::Verified(0, 0),
					crate::verifier::Event::VerificationFailed(0, FeasibilityError::InvalidScore),
				]
			);

			// Check that expected events were emitted for the rejection
			assert_eq!(
				signed_events_since_last_call(),
				vec![Event::Registered(0, 99, invalid_score), Event::Slashed(0, 99, 5),]
			);

			// Verify no-restart conditions are met
			assert!(MultiBlock::current_phase().is_signed_validation());
			assert!(!Submissions::<Runtime>::has_leader(current_round));
			assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 0);
			assert_eq!(VerifierPallet::status(), crate::verifier::Status::Nothing);

			// Verifier should remain idle for the rest of the signed validation phase
			while crate::Pallet::<Runtime>::current_phase().is_signed_validation() {
				roll_next_and_verifier(Status::Nothing);
			}

			roll_next_and_phase(Phase::<T>::Done);
			assert_eq!(VerifierPallet::status(), crate::verifier::Status::Nothing);
		});
	}

	#[test]
	fn missing_pages_treated_as_empty() {
		// Test the scenario where a valid multi-page solution is mined but only some pages
		// are submitted.
		//
		// The key behavior being tested: missing pages should be treated as empty/default
		// rather than causing verification failures.
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			let paged = mine_full_solution().unwrap();
			let real_score = paged.score;
			let submitter = 99;

			assert!(
				paged.solution_pages.len() > 1,
				"Test requires a multi-page solution, got {} pages",
				paged.solution_pages.len()
			);

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(submitter), real_score));

			// Submit ONLY page 0 of the solution, deliberately skip pages 1 and 2.
			let first_page = paged.solution_pages.pagify(Pages::get()).next().unwrap();
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(submitter),
				first_page.0,
				Some(Box::new(first_page.1.clone()))
			));

			// Verify the metadata shows correct page submission status
			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, submitter).unwrap().pages.into_inner(),
				vec![true, false, false]
			);

			// Verify that the solution data provider can access all pages without errors
			let page_0 = SignedPallet::get_page(0);
			let page_1 = SignedPallet::get_page(1);
			let page_2 = SignedPallet::get_page(2);

			// Page 0 should have actual data, pages 1 and 2 should be empty (default)
			assert_ne!(page_0, Default::default(), "Submitted page 0 should have data");
			assert_eq!(page_1, Default::default(), "Missing page 1 should return empty solution");
			assert_eq!(page_2, Default::default(), "Missing page 2 should return empty solution");

			// Start verification process
			roll_to_signed_validation_open();

			// The verification should proceed and treat the missing pages as empty
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(5), Status::Ongoing(1));
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(4), Status::Ongoing(0));
			roll_next_and_phase_verifier(Phase::<T>::SignedValidation(3), Status::Nothing);

			// Check only the events from this verification
			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					crate::verifier::Event::Verified(2, 0), // Page 2 missing, treated as empty
					crate::verifier::Event::Verified(1, 0), // Page 1 missing, treated as empty
					crate::verifier::Event::Verified(0, 2), // Page 0 submitted with real data
					crate::verifier::Event::VerificationFailed(0, FeasibilityError::InvalidScore)
				],
				"Missing pages should be treated as empty, but partial submission leads to verification failure due to score mismatch"
			);
		});
	}

	#[test]
	fn not_all_solutions_verified_signed_verification_to_unsigned() {
		// Test that in case of multiple verifications when not all of them are verified within the
		// signed validation phase, the verifier should stop and go back to idle when transitioning
		// from signed validation to unsigned phase, while keeping not yet verified solutions.
		// NOTE: the signed validation phase must be a multiple of the number of pages, which
		// ensures that solutions cannot be halfway verified
		ExtBuilder::signed()
			.pages(3)
			.signed_validation_phase(3) // so that we can validate only one solution per validation phase
			.unsigned_phase(3)
			.build_and_execute(|| {
				roll_to_signed_open();

				// Submit invalid solution with high score but no pages (will be slashed)
				{
					let mut strong_score = mine_full_solution().unwrap().score;
					strong_score.minimal_stake *= 2;
					assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), strong_score));
					// Don't submit any pages - this will cause it to be slashed
				}

				// Submit good solution with all pages
				{
					let strong_solution = mine_full_solution().unwrap();
					load_signed_for_verification(999, strong_solution.clone());
				}

				assert_eq!(
					signed_events_since_last_call(),
					vec![
						SignedEvent::Registered(
							0,
							99,
							ElectionScore {
								minimal_stake: 110,
								sum_stake: 130,
								sum_stake_squared: 8650
							}
						),
						SignedEvent::Registered(
							0,
							999,
							ElectionScore {
								minimal_stake: 55,
								sum_stake: 130,
								sum_stake_squared: 8650
							}
						),
						SignedEvent::Stored(0, 999, 0),
						SignedEvent::Stored(0, 999, 1),
						SignedEvent::Stored(0, 999, 2),
					]
				);

				let current_round = SignedPallet::current_round();
				assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 2);

				roll_to_signed_validation_open();
				assert_eq!(verifier_events_since_last_call(), vec![]);

				roll_next_and_phase_verifier(Phase::SignedValidation(2), Status::Ongoing(1));
				assert_eq!(
					verifier_events_since_last_call(),
					vec![crate::verifier::Event::Verified(2, 0)]
				);

				// Initial verifier state should have no queued solution
				assert_eq!(VerifierPallet::queued_score(), None);

				// Bad solution starts verification
				roll_next_and_phase_verifier(Phase::SignedValidation(1), Status::Ongoing(0));
				assert_eq!(
					verifier_events_since_last_call(),
					vec![crate::verifier::Event::Verified(1, 0)]
				);

				roll_next_and_phase_verifier(Phase::SignedValidation(0), Status::Nothing);
				assert_eq!(
					verifier_events_since_last_call(),
					vec![
						crate::verifier::Event::Verified(0, 0),
						crate::verifier::Event::VerificationFailed(
							0,
							FeasibilityError::InvalidScore
						)
					]
				);

				roll_next_and_phase(Phase::Unsigned(UnsignedPhase::get() - 1));

				// Check that invalid solution is slashed but good solution remains
				assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 1);
				assert!(Submissions::<Runtime>::has_leader(current_round));

				// Check that expected events were emitted for the rejection
				assert_eq!(signed_events_since_last_call(), vec![Event::Slashed(0, 99, 5)]);

				// Verify no additional events occurred
				assert!(
					verifier_events_since_last_call().is_empty(),
					"No additional verifier events should occur"
				);

				// Verifier should be STOPPED when transitioning to unsigned
				assert_eq!(VerifierPallet::status(), crate::verifier::Status::Nothing);

				// Verify no solution was queued (verification was stopped, not completed)
				assert_eq!(VerifierPallet::queued_score(), None);
			});
	}

	#[test]
	fn incomplete_signed_verification_1_solution_back_to_signed_then_done() {
		ExtBuilder::signed()
			.pages(3)
			.signed_validation_phase(3)
			.are_we_done(AreWeDoneModes::BackToSigned)
			.build_and_execute(|| {
				roll_to_signed_open();

				// submit a bad solution with junk in page 2
				{
					let mut bad_solution = mine_full_solution().unwrap();
					bad_solution.solution_pages[1usize].corrupt();
					load_signed_for_verification(99, bad_solution);
				}

				// submit a good solution that will be sent to next round as we won't have enough
				// time for it.
				{
					let good_solution = mine_full_solution().unwrap();
					load_signed_for_verification(999, good_solution);
				}
				// skip registration events
				let _ = signed_events_since_last_call();

				roll_to_signed_validation_open();

				// henceforth we proceed block-by-block for better visibility of what is happening.

				// 2 blocks to reject the first one
				roll_next_and_phase_verifier(Phase::SignedValidation(2), Status::Ongoing(1));
				roll_next_and_phase_verifier(Phase::SignedValidation(1), Status::Nothing);

				assert_eq!(
					verifier_events_since_last_call(),
					vec![
						crate::verifier::Event::Verified(2, 2),
						crate::verifier::Event::VerificationFailed(
							1,
							FeasibilityError::NposElection(
								sp_npos_elections::Error::SolutionInvalidIndex
							)
						),
					]
				);
				assert_eq!(
					signed_events_since_last_call(),
					vec![crate::signed::Event::Slashed(0, 99, 8)]
				);

				// we have 2 blocks left in signed verification (1 and 3), but we cannot do anything
				// here. Status remains idle.
				roll_next_and_phase_verifier(Phase::SignedValidation(0), Status::Nothing);

				// we go back to signed next
				roll_next_and_phase(Phase::Signed(4));

				// no one submits again, and we go to verification again
				roll_to_signed_validation_open();

				// 4 block to verify: 1 to set status, and 3 to verify
				roll_next_and_phase_verifier(Phase::SignedValidation(2), Status::Ongoing(1));
				roll_next_and_phase_verifier(Phase::SignedValidation(1), Status::Ongoing(0));
				roll_next_and_phase_verifier(Phase::SignedValidation(0), Status::Nothing);

				assert_eq!(
					verifier_events_since_last_call(),
					vec![
						crate::verifier::Event::Verified(2, 2),
						crate::verifier::Event::Verified(1, 2),
						crate::verifier::Event::Verified(0, 2),
						crate::verifier::Event::Queued(
							ElectionScore {
								minimal_stake: 55,
								sum_stake: 130,
								sum_stake_squared: 8650
							},
							None
						)
					]
				);
				assert_eq!(
					signed_events_since_last_call(),
					vec![crate::signed::Event::Rewarded(0, 999, 7)]
				);

				// verifier is `Nothing`, and will remain so as signed-pallet will not start it
				// again.
				assert_eq!(crate::Pallet::<T>::current_phase(), Phase::SignedValidation(0));
				assert_eq!(VerifierPallet::status(), crate::verifier::Status::Nothing);

				// next block we go to done
				roll_next_and_phase_verifier(Phase::Done, Status::Nothing);
			})
	}

	#[test]
	fn max_queue_with_single_valid_solution_at_end() {
		// Test that when the submission queue is at capacity (MaxSubmissions = 3), with only the
		// third submission being valid, the verifier processes the entire queue sequentially and
		// eventually accepts the final valid solution.
		ExtBuilder::signed()
			.max_signed_submissions(3)
			.signed_validation_phase(9) // 3 solutions * 3 pages per solution = 9 blocks needed
			.build_and_execute(|| {
				roll_to_signed_open();
				assert_full_snapshot();

				let current_round = SignedPallet::current_round();

				// Submit two invalid solutions (register but don't submit pages)
				{
					let invalid_score1 = {
						let mut score = mine_full_solution().unwrap().score;
						score.minimal_stake *= 2; // Make it attractive but it will be invalid
						score
					};
					assert_ok!(SignedPallet::register(RuntimeOrigin::signed(91), invalid_score1));
				}

				{
					let invalid_score2 = {
						let mut score = mine_full_solution().unwrap().score;
						score.minimal_stake *= 3; // Make it even more attractive but still invalid
						score
					};
					assert_ok!(SignedPallet::register(RuntimeOrigin::signed(92), invalid_score2));
				}

				// Submit one valid solution at the end
				let valid_score = {
					let valid_solution = mine_full_solution().unwrap();
					load_signed_for_verification(99, valid_solution.clone());
					valid_solution.score
				};
				// skip registration events
				let _ = signed_events_since_last_call();

				// Verify we have 3 submissions at max capacity
				assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 3);
				assert!(Submissions::<Runtime>::has_leader(current_round));

				// Move to verification phase
				roll_to_signed_validation_open();

				roll_next_and_phase_verifier(Phase::SignedValidation(8), Status::Ongoing(1));
				roll_next_and_phase_verifier(Phase::SignedValidation(7), Status::Ongoing(0));
				// Note `Status::Ongoing(2)` -- we are signaled to start again in then next block.
				roll_next_and_phase_verifier(Phase::SignedValidation(6), Status::Ongoing(2));

				// Check events after first solution (91) is rejected
				assert_eq!(
					verifier_events_since_last_call(),
					vec![
						crate::verifier::Event::Verified(2, 0),
						crate::verifier::Event::Verified(1, 0),
						crate::verifier::Event::Verified(0, 0),
						crate::verifier::Event::VerificationFailed(
							0,
							FeasibilityError::InvalidScore
						),
					]
				);
				assert_eq!(signed_events_since_last_call(), vec![SignedEvent::Slashed(0, 92, 5)]);

				// Verify first solution was slashed and removed
				assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 2);

				roll_next_and_phase_verifier(Phase::SignedValidation(5), Status::Ongoing(1));
				roll_next_and_phase_verifier(Phase::SignedValidation(4), Status::Ongoing(0));
				// Note `Status::Ongoing(2)` -- we are signaled to start again in then next block.
				roll_next_and_phase_verifier(Phase::SignedValidation(3), Status::Ongoing(2));

				// Check events after second solution (92) is rejected
				assert_eq!(
					verifier_events_since_last_call(),
					vec![
						crate::verifier::Event::Verified(2, 0),
						crate::verifier::Event::Verified(1, 0),
						crate::verifier::Event::Verified(0, 0),
						crate::verifier::Event::VerificationFailed(
							0,
							FeasibilityError::InvalidScore
						),
					]
				);
				assert_eq!(signed_events_since_last_call(), vec![SignedEvent::Slashed(0, 91, 5)]);

				// Verify second solution was slashed and removed
				assert_eq!(Submissions::<Runtime>::submitters_count(current_round), 1);

				// Verify the last remaining solution is our valid one
				let leader = Submissions::<Runtime>::leader(current_round).unwrap();
				assert_eq!(leader.0, 99);

				// Process the final valid solution. We are done _exactly on time_.
				roll_next_and_phase_verifier(Phase::SignedValidation(2), Status::Ongoing(1));
				roll_next_and_phase_verifier(Phase::SignedValidation(1), Status::Ongoing(0));
				roll_next_and_phase_verifier(Phase::SignedValidation(0), Status::Nothing);

				// Check events after valid solution (99) is accepted
				assert_eq!(
					verifier_events_since_last_call(),
					vec![
						crate::verifier::Event::Verified(2, 2),
						crate::verifier::Event::Verified(1, 2),
						crate::verifier::Event::Verified(0, 2),
						crate::verifier::Event::Queued(valid_score, None),
					]
				);

				// Check that all expected events were emitted in the correct order
				assert_eq!(signed_events_since_last_call(), vec![Event::Rewarded(0, 99, 7)]);

				// finally done
				roll_next_and_phase_verifier(Phase::Done, Status::Nothing);
				assert!(
					verifier_events_since_last_call().is_empty(),
					"No additional verifier events should occur"
				);
			});
	}
}

mod invulnerables {
	use frame_support::hypothetically;

	use super::*;

	fn make_invulnerable(who: AccountId) {
		SignedPallet::set_invulnerables(RuntimeOrigin::root(), vec![who]).unwrap();
	}

	#[test]
	fn set_invulnerables_requires_admin_origin() {
		ExtBuilder::signed().build_and_execute(|| {
			// Should fail with non-admin origin
			assert_noop!(
				SignedPallet::set_invulnerables(RuntimeOrigin::signed(1), vec![99]),
				sp_runtime::DispatchError::BadOrigin
			);

			// Should succeed with admin origin (root)
			assert_ok!(SignedPallet::set_invulnerables(RuntimeOrigin::root(), vec![99]));
		});
	}

	#[test]
	fn set_invulnerables_too_many_fails() {
		ExtBuilder::signed().build_and_execute(|| {
			// Try to set more than 16 invulnerables (ConstU32<16> limit)
			let too_many: Vec<AccountId> = (1..=17).collect();
			assert_noop!(
				SignedPallet::set_invulnerables(RuntimeOrigin::root(), too_many),
				Error::<Runtime>::TooManyInvulnerables
			);
		});
	}

	#[test]
	fn invulnerable_pays_different_deposit_independent_of_pages() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			make_invulnerable(99);
			assert_eq!(balances(99), (100, 0));
			assert_eq!(<Runtime as crate::signed::Config>::InvulnerableDeposit::get(), 7);

			let score = ElectionScore { minimal_stake: 100, ..Default::default() };

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_eq!(balances(99), (93, 7));

			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 7,
					// ^^ fixed deposit
					fee: 1,
					// ^^ fee accumulates
					pages: bounded_vec![false, false, false],
					reward: 3
				}
			);

			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				0,
				Some(Default::default())
			));

			assert_eq!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata {
					claimed_score: score,
					deposit: 7,
					// ^^ deposit still fixed at 7
					fee: 2,
					// ^^ fee accumulates
					pages: bounded_vec![true, false, false],
					reward: 3
				}
			);

			// Submit additional pages to verify deposit remains fixed
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				1,
				Some(Default::default())
			));
			assert_ok!(SignedPallet::submit_page(
				RuntimeOrigin::signed(99),
				2,
				Some(Default::default())
			));

			// Verify final state after all pages are submitted
			let final_metadata = Submissions::<Runtime>::metadata_of(0, 99).unwrap();
			assert_eq!(
				final_metadata,
				SubmissionMetadata {
					claimed_score: score,
					deposit: 7,
					// ^^ deposit still fixed at 7 even after submitting all pages
					fee: 4,
					// ^^ fee accumulates: register(1) + page0(1) + page1(1) + page2(1) = 4 total
					pages: bounded_vec![true, true, true],
					// ^^ all pages submitted
					reward: 3
				}
			);

			// Balance should remain the same - only deposit is held, fees tracked in metadata
			assert_eq!(balances(99), (93, 7));
		})
	}

	#[test]
	fn multiple_invulnerables_all_get_fixed_deposit() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			// Set multiple invulnerables
			SignedPallet::set_invulnerables(RuntimeOrigin::root(), vec![99, 98, 97]).unwrap();

			let score = ElectionScore { minimal_stake: 100, ..Default::default() };

			// All should pay the same fixed deposit
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(98), score));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(97), score));

			// All should have paid exactly InvulnerableDeposit (7)
			assert_eq!(balances(99), (93, 7));
			assert_eq!(balances(98), (93, 7));
			assert_eq!(balances(97), (93, 7));
		});
	}

	#[test]
	fn invulnerable_cannot_eject_first() {
		ExtBuilder::signed().max_signed_submissions(3).build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();
			make_invulnerable(99);

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score_from(100)));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(98), score_from(150)));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(97), score_from(200)));

			assert_eq!(
				Submissions::<Runtime>::leaderboard(0),
				vec![(99, score_from(100)), (98, score_from(150)), (97, score_from(200))]
			);

			// inserting someone who would eject 99 won't work.
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(96), score_from(100)),
				Error::<T>::QueueFull,
			);
			// inserting someone who would eject 98 will work.
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(96), score_from(155)));
			assert_eq!(
				Submissions::<Runtime>::leaderboard(0),
				vec![(99, score_from(100)), (96, score_from(155)), (97, score_from(200))]
			);
		})
	}

	#[test]
	fn invulnerable_cannot_eject_middle() {
		ExtBuilder::signed().max_signed_submissions(3).build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();
			make_invulnerable(99);

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score_from(150)));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(98), score_from(100)));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(97), score_from(200)));

			assert_eq!(
				Submissions::<Runtime>::leaderboard(0),
				vec![(98, score_from(100)), (99, score_from(150)), (97, score_from(200))]
			);

			// worse than all
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(96), score_from(50)),
				Error::<T>::QueueFull,
			);

			// better than our first item
			hypothetically!({
				assert_ok!(SignedPallet::register(RuntimeOrigin::signed(96), score_from(125)));
				assert_eq!(
					Submissions::<Runtime>::leaderboard(0),
					vec![(96, score_from(125)), (99, score_from(150)), (97, score_from(200))]
				);
			});

			// better than inv
			hypothetically!({
				assert_ok!(SignedPallet::register(RuntimeOrigin::signed(96), score_from(175)));
				assert_eq!(
					Submissions::<Runtime>::leaderboard(0),
					vec![(99, score_from(150)), (96, score_from(175)), (97, score_from(200))]
				);
			});

			// better than all
			hypothetically!({
				assert_ok!(SignedPallet::register(RuntimeOrigin::signed(96), score_from(225)));
				assert_eq!(
					Submissions::<Runtime>::leaderboard(0),
					vec![(99, score_from(150)), (97, score_from(200)), (96, score_from(225))]
				);
			});
		})
	}

	#[test]
	fn all_invulnerables() {
		ExtBuilder::signed().max_signed_submissions(3).build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();
			assert_ok!(SignedPallet::set_invulnerables(RuntimeOrigin::root(), vec![99, 98, 97]));

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score_from(150)));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(98), score_from(100)));
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(97), score_from(200)));

			// Nothing can touch our boys now.
			assert_noop!(
				SignedPallet::register(RuntimeOrigin::signed(96), score_from(1000)),
				Error::<T>::QueueFull,
			);
		})
	}

	#[test]
	fn discarded_invulnerable_gets_fee_and_deposit_back() {
		ExtBuilder::signed().build_and_execute(|| {
			make_invulnerable(99);

			roll_to_signed_open();

			// a weak, discarded solution from 99
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), Default::default()));
			assert!(matches!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap(),
				SubmissionMetadata { deposit: 7, fee: 1, .. }
			));
			// note: we don't actually collect the tx-fee in the tests
			assert_eq!(balances(99), (93, 7));

			// a valid, strong solution.
			let paged = mine_full_solution().unwrap();
			load_signed_for_verification(98, paged.clone());
			let _ = signed_events_since_last_call();

			roll_to_signed_validation_open();
			roll_to_full_verification();

			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					crate::verifier::Event::Verified(2, 2),
					crate::verifier::Event::Verified(1, 2),
					crate::verifier::Event::Verified(0, 2),
					crate::verifier::Event::Queued(
						ElectionScore {
							minimal_stake: 55,
							sum_stake: 130,
							sum_stake_squared: 8650
						},
						None
					)
				]
			);
			assert_eq!(signed_events_since_last_call(), vec![Event::Rewarded(0, 98, 7)]);

			// not relevant: signed will not start verification again
			roll_next_and_phase_verifier(Phase::SignedValidation(2), Status::Nothing);
			assert!(verifier_events_since_last_call().is_empty());

			// fast-forward to round being over
			roll_to_done();
			MultiBlock::rotate_round();

			// now we can delete our stuff.
			assert_ok!(SignedPallet::clear_old_round_data(
				RuntimeOrigin::signed(99),
				0,
				Pages::get()
			));
			assert_eq!(signed_events_since_last_call(), vec![Event::Discarded(0, 99)]);
			// full deposit is returned + tx-fee
			assert_eq!(balances(99), (101, 0));
		})
	}

	#[test]
	fn discarded_invulnerable_fee_refund_is_bounded_by_max_fee_refund() {
		ExtBuilder::signed().build_and_execute(|| {
			// GIVEN an invulnerable with an accrued fee above the bound.
			make_invulnerable(99);
			roll_to_signed_open();

			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), Default::default()));
			for _ in 0..6 {
				assert_ok!(SignedPallet::submit_page(RuntimeOrigin::signed(99), 0, None));
				assert_ok!(SignedPallet::submit_page(
					RuntimeOrigin::signed(99),
					0,
					Some(Default::default())
				));
			}
			assert!(
				Submissions::<Runtime>::metadata_of(0, 99).unwrap().fee > max_fee_refund(),
				"the bound must actually bind for this test to mean anything"
			);

			// a stronger solution wins the round, so 99 is discarded rather than rewarded.
			load_signed_for_verification(98, mine_full_solution().unwrap());
			roll_to_signed_validation_open();
			roll_to_full_verification();
			roll_to_done();
			MultiBlock::rotate_round();
			assert!(signed_events_since_last_call().contains(&Event::Rewarded(
				0,
				98,
				SignedRewardBase::get() + max_fee_refund()
			)));

			// WHEN they clear their discarded submission.
			assert_ok!(SignedPallet::clear_old_round_data(
				RuntimeOrigin::signed(99),
				0,
				Pages::get()
			));

			// THEN they are discarded, their deposit is returned in full, and the fee refund is
			// the bound.
			assert_eq!(signed_events_since_last_call(), vec![Event::Discarded(0, 99)]);
			assert_eq!(balances(99), (100 + max_fee_refund(), 0));
		})
	}

	#[test]
	fn removing_from_invulnerables_affects_future_submissions() {
		ExtBuilder::signed().build_and_execute(|| {
			// Initially make invulnerable
			make_invulnerable(99);

			roll_to_signed_open();
			assert_full_snapshot();

			let score = ElectionScore { minimal_stake: 100, ..Default::default() };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_eq!(Submissions::<Runtime>::metadata_of(0, 99).unwrap().deposit, 7);

			// Clear round and remove from invulnerables
			roll_to_done();
			MultiBlock::rotate_round();
			SignedPallet::set_invulnerables(RuntimeOrigin::root(), vec![]).unwrap();

			// Start new election
			assert_ok!(MultiBlock::start());

			// Now roll to signed open for the new round
			roll_to_signed_open();
			assert_full_snapshot();
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			let new_deposit =
				Submissions::<Runtime>::metadata_of(MultiBlock::round(), 99).unwrap().deposit;
			assert_eq!(new_deposit, 5); // Should not be fixed deposit anymore
		});
	}

	#[test]
	fn slashed_invulnerable_is_not_expelled() {
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();
			make_invulnerable(99);
			assert!(Invulnerables::<T>::get().contains(&99));

			let invalid_score = score_from(777);
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), invalid_score));

			roll_to_signed_validation_open();
			roll_to_full_verification();

			// Check that rejection events were properly generated
			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					crate::verifier::Event::Verified(2, 0), // empty page
					crate::verifier::Event::Verified(1, 0), // empty page
					crate::verifier::Event::Verified(0, 0), // empty page
					crate::verifier::Event::VerificationFailed(0, FeasibilityError::InvalidScore),
				]
			);

			// Check that expected events were emitted for the rejection
			assert_eq!(
				signed_events(),
				vec![Event::Registered(0, 99, invalid_score), Event::Slashed(0, 99, 7)] /* slash
				                                                                         * amount
				                                                                         * is indeed
				                                                                         * the invulnerable
				                                                                         * deposit
				                                                                         * (7) ^^^^ */
			);

			// Verify invulnerable is not expelled. It remains in the list despite being slashed.
			assert!(Invulnerables::<T>::get().contains(&99));
		});
	}
}

mod defensive_tests {
	use super::*;

	#[test]
	#[cfg(debug_assertions)]
	#[should_panic(expected = "Defensive failure has been triggered!")]
	fn missing_leader_storage_triggers_defensive() {
		// Call Verifier::start and mutate storage to delete the score of the leader.
		// This creates the scenario where score becomes unavailable during verification
		ExtBuilder::signed().build_and_execute(|| {
			// Setup a leader first
			roll_to_signed_open();
			assert_eq!(balances(99), (100, 0));
			let score = ElectionScore { minimal_stake: 100, ..Default::default() };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), score));
			assert_eq!(balances(99), (95, 5)); // deposit taken

			// Submit a solution page to have something to verify
			assert_ok!(SignedPallet::submit_page(RuntimeOrigin::signed(99), 0, Default::default()));
			assert_eq!(
				signed_events_since_last_call(),
				vec![
					Event::Registered(
						0,
						99,
						ElectionScore { minimal_stake: 100, sum_stake: 0, sum_stake_squared: 0 }
					),
					Event::Stored(0, 99, 0)
				]
			);

			// verify 2 of the pages.
			roll_to_signed_validation_open();
			roll_next_and_phase_verifier(Phase::SignedValidation(5), Status::Ongoing(1));
			roll_next_and_phase_verifier(Phase::SignedValidation(4), Status::Ongoing(0));

			// Delete the score storage
			let full_key = crate::signed::pallet::SortedScores::<Runtime>::hashed_key_for(
				SignedPallet::current_round(),
			);
			unhashed::kill(&full_key);

			// Complete verification - this should trigger score unavailable detection
			roll_next_and_phase(Phase::SignedValidation(3));
		});
	}

	#[test]
	#[should_panic(expected = "Defensive failure has been triggered!")]
	fn get_score_defensive_when_no_leader() {
		// Test that get_score() triggers defensive failure when no leader exists
		ExtBuilder::signed().build_and_execute(|| {
			// Ensure we're in signed phase but no submissions exist
			roll_to_signed_open();

			// Verify no leader exists
			assert_eq!(Submissions::<Runtime>::leader(SignedPallet::current_round()), None);

			// get_score should trigger defensive failure when no leader exists
			let _score = SignedPallet::get_score();
		});
	}

	#[test]
	#[should_panic(expected = "Defensive failure has been triggered!")]
	fn get_page_defensive_when_no_leader() {
		// Test that get_page() triggers defensive failure when no leader exists
		ExtBuilder::signed().build_and_execute(|| {
			// Ensure we're in signed phase but no submissions exist
			roll_to_signed_open();

			// Verify no leader exists
			assert_eq!(Submissions::<Runtime>::leader(SignedPallet::current_round()), None);

			// get_page should trigger defensive failure when no leader exists
			let _page = SignedPallet::get_page(0);
		});
	}
}

mod issuance {
	use super::*;
	use frame_support::traits::fungible::Mutate;

	/// Arbitrary account used as a reward pot in these tests.
	const POT: AccountId = 1000;

	/// Submit a full valid solution for `who` and roll through to the Rewarded event.
	///
	/// Returns the balances snapshot of `who` just before rolling to SignedValidation.
	fn submit_and_verify_winning(who: AccountId) {
		roll_to_signed_open();
		assert_full_snapshot();

		let paged = mine_full_solution().unwrap();
		load_signed_for_verification(who, paged);
		let _ = signed_events_since_last_call();

		roll_to_signed_validation_open();
		let _ = multi_block_events_since_last_call();

		for _ in 0..Pages::get() + 1 {
			roll_to(System::block_number() + 1);
		}
	}

	#[test]
	fn reward_mints_by_default() {
		// When RewardSource is None (default), rewards are minted and TotalIssuance increases.
		ExtBuilder::signed().build_and_execute(|| {
			SignedRewardSource::set(None);

			let ti_before = Balances::total_issuance();
			submit_and_verify_winning(999);

			let rewarded_events: Vec<_> = signed_events_since_last_call()
				.into_iter()
				.filter(|e| matches!(e, SignedEvent::Rewarded(..)))
				.collect();
			assert_eq!(rewarded_events.len(), 1, "expected exactly one Rewarded event");

			let ti_after = Balances::total_issuance();
			// reward (3) + fee (1 register + 3 pages = 4) = 7
			assert!(ti_after > ti_before, "total issuance must increase when minting rewards");
		});
	}

	#[test]
	fn reward_transfers_from_source_when_configured() {
		// When RewardSource is Some(pot), the reward is transferred from the pot account.
		// TotalIssuance must not change during the reward payment.
		ExtBuilder::signed().build_and_execute(|| {
			Balances::mint_into(&POT, 1_000).unwrap();
			SignedRewardSource::set(Some(POT));

			let ti_before = Balances::total_issuance();
			let pot_before = Balances::free_balance(POT);

			submit_and_verify_winning(999);

			let rewarded_events: Vec<_> = signed_events_since_last_call()
				.into_iter()
				.filter(|e| matches!(e, SignedEvent::Rewarded(..)))
				.collect();
			assert_eq!(rewarded_events.len(), 1, "expected exactly one Rewarded event");

			// mint_into of pot balance was the only issuance change before the test window
			let ti_after = Balances::total_issuance();
			let pot_after = Balances::free_balance(POT);

			assert_eq!(ti_before, ti_after, "total issuance must not change when paying from pot");
			assert!(pot_after < pot_before, "reward pot balance must decrease after paying winner");
		});
	}

	#[test]
	fn reward_deferred_when_source_pot_is_empty() {
		// Empty pot: transfer fails, RewardPaymentDeferred fires instead of Rewarded, and the
		// amount is queued in UnpaidRewards rather than paid.
		ExtBuilder::signed().build_and_execute(|| {
			// Do NOT fund the pot.
			SignedRewardSource::set(Some(POT));

			let ti_before = Balances::total_issuance();
			submit_and_verify_winning(999);
			let ti_after = Balances::total_issuance();

			let events = signed_events_since_last_call();
			assert!(
				!events.iter().any(|e| matches!(e, SignedEvent::Rewarded(..))),
				"Rewarded must not be emitted when the pot transfer fails"
			);
			let deferred_events: Vec<_> = events
				.into_iter()
				.filter(|e| matches!(e, SignedEvent::RewardPaymentDeferred(..)))
				.collect();
			assert_eq!(
				deferred_events.len(),
				1,
				"expected exactly one RewardPaymentDeferred event"
			);
			assert_eq!(ti_before, ti_after, "total issuance must not change when pot is empty");
			assert_eq!(UnpaidRewards::<T>::get().len(), 1, "the reward must be queued");
		});
	}

	#[test]
	fn claim_unpaid_reward_works_once_pot_refilled() {
		ExtBuilder::signed().build_and_execute(|| {
			SignedRewardSource::set(Some(POT));

			submit_and_verify_winning(999);
			assert_eq!(UnpaidRewards::<T>::get().len(), 1);
			let _ = signed_events_since_last_call();

			// Refill the pot, then claim.
			Balances::mint_into(&POT, 1_000).unwrap();
			let balance_before = Balances::free_balance(999);

			assert_ok!(SignedPallet::claim_unpaid_reward(RuntimeOrigin::signed(999), 0));

			assert!(
				Balances::free_balance(999) > balance_before,
				"winner must receive the deferred reward"
			);
			assert!(UnpaidRewards::<T>::get().is_empty(), "claimed entry must be removed");
			assert!(signed_events_since_last_call()
				.iter()
				.any(|e| matches!(e, SignedEvent::Rewarded(0, 999, _))));
		});
	}

	#[test]
	fn claim_unpaid_reward_is_permissionless() {
		// Anyone can trigger the claim; the payout still goes to the original winner, not the
		// caller.
		ExtBuilder::signed().build_and_execute(|| {
			SignedRewardSource::set(Some(POT));

			submit_and_verify_winning(999);
			Balances::mint_into(&POT, 1_000).unwrap();
			let winner_balance_before = Balances::free_balance(999);
			let caller_balance_before = Balances::free_balance(1);

			// 1 claims on behalf of 999.
			assert_ok!(SignedPallet::claim_unpaid_reward(RuntimeOrigin::signed(1), 0));

			assert!(Balances::free_balance(999) > winner_balance_before, "winner gets paid");
			assert_eq!(Balances::free_balance(1), caller_balance_before, "caller gets nothing");
		});
	}

	#[test]
	fn claim_unpaid_reward_fails_when_no_entry() {
		ExtBuilder::signed().build_and_execute(|| {
			assert_noop!(
				SignedPallet::claim_unpaid_reward(RuntimeOrigin::signed(999), 0),
				Error::<T>::NoUnpaidReward
			);
		});
	}

	#[test]
	fn claim_unpaid_reward_fails_when_pot_still_depleted() {
		ExtBuilder::signed().build_and_execute(|| {
			SignedRewardSource::set(Some(POT));
			submit_and_verify_winning(999);
			assert_eq!(UnpaidRewards::<T>::get().len(), 1);

			// Pot is still empty.
			assert_noop!(
				SignedPallet::claim_unpaid_reward(RuntimeOrigin::signed(999), 0),
				Error::<T>::PotStillDepleted
			);
			assert_eq!(UnpaidRewards::<T>::get().len(), 1, "entry must remain queued");
		});
	}

	#[test]
	fn reward_deferral_evicts_oldest_when_unpaid_queue_is_full() {
		ExtBuilder::signed().build_and_execute(|| {
			SignedRewardSource::set(Some(POT));

			// Fill the unpaid-rewards queue to capacity, oldest (round 999) first.
			UnpaidRewards::<T>::mutate(|unpaid| {
				for i in 0..MAX_UNPAID_REWARDS as u64 {
					unpaid
						.try_push(UnpaidReward { round: 999 + i as u32, who: i, amount: 1 })
						.unwrap();
				}
			});

			submit_and_verify_winning(999);

			let events = signed_events_since_last_call();
			assert!(
				events.iter().any(|e| matches!(e, SignedEvent::UnpaidRewardEvicted(999, 0, 1))),
				"expected the oldest entry (round 999) to be evicted"
			);
			assert!(
				events.iter().any(|e| matches!(e, SignedEvent::RewardPaymentDeferred(..))),
				"the new reward must still be deferred, never dropped"
			);
			let unpaid = UnpaidRewards::<T>::get();
			assert_eq!(
				unpaid.len(),
				MAX_UNPAID_REWARDS as usize,
				"queue stays at capacity, oldest swapped for newest"
			);
			assert!(!unpaid.iter().any(|e| e.round == 999), "evicted entry must be gone");
		});
	}

	#[test]
	fn fee_refund_failed_when_source_pot_is_empty() {
		// An invulnerable's fee refund is not deferred like a reward: on failure it just emits
		// FeeRefundFailed, and Discarded still fires normally.
		ExtBuilder::signed().build_and_execute(|| {
			SignedRewardSource::set(Some(POT));
			// Do NOT fund POT.
			SignedPallet::set_invulnerables(RuntimeOrigin::root(), vec![99]).unwrap();

			roll_to_signed_open();
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), Default::default()));

			let paged = mine_full_solution().unwrap();
			load_signed_for_verification(98, paged.clone());
			let _ = signed_events_since_last_call();

			roll_to_signed_validation_open();
			roll_to_full_verification();
			let _ = signed_events_since_last_call();
			let _ = verifier_events_since_last_call();

			roll_next_and_phase_verifier(Phase::SignedValidation(2), Status::Nothing);
			let _ = verifier_events_since_last_call();

			roll_to_done();
			MultiBlock::rotate_round();

			assert_ok!(SignedPallet::clear_old_round_data(
				RuntimeOrigin::signed(99),
				0,
				Pages::get()
			));

			let events = signed_events_since_last_call();
			assert!(
				events.iter().any(|e| matches!(e, SignedEvent::FeeRefundFailed(0, 99, _))),
				"expected FeeRefundFailed when the pot can't pay the fee refund"
			);
			assert!(events.iter().any(|e| matches!(e, SignedEvent::Discarded(0, 99))));
			assert!(!events.iter().any(|e| matches!(e, SignedEvent::Rewarded(..))));
			// Full deposit (7) is still returned; the 1-unit fee refund is the part that failed.
			assert_eq!(balances(99), (100, 0));
		});
	}

	#[test]
	fn slash_burns_deposit_by_default() {
		// When SignedSlashTarget is None, slashed deposits are burned (credit is dropped).
		// TotalIssuance must decrease by the slashed amount.
		ExtBuilder::signed().build_and_execute(|| {
			roll_to_signed_open();
			assert_full_snapshot();

			let invalid_score =
				ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 };
			// 99 registers with 5 deposit (SignedDepositBase) but submits no valid pages.
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), invalid_score));

			let ti_before = Balances::total_issuance();

			roll_to_signed_validation_open();
			roll_to_full_verification();

			// Slashed event emitted and deposit is burned.
			assert!(signed_events_since_last_call()
				.iter()
				.any(|e| matches!(e, SignedEvent::Slashed(..))));

			let ti_after = Balances::total_issuance();
			assert!(ti_after < ti_before, "total issuance must decrease when slash is burned");
		});
	}

	#[test]
	fn slash_transfers_to_target_when_configured() {
		// When SignedSlashTarget is Some(target), slashed deposits are transferred to target
		// instead of being burned. TotalIssuance must not change.
		ExtBuilder::signed().build_and_execute(|| {
			SignedSlashTarget::set(Some(POT));

			roll_to_signed_open();
			assert_full_snapshot();

			let invalid_score =
				ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 };
			assert_ok!(SignedPallet::register(RuntimeOrigin::signed(99), invalid_score));

			let ti_before = Balances::total_issuance();
			let target_before = Balances::free_balance(POT);

			roll_to_signed_validation_open();
			roll_to_full_verification();

			let slashed_events: Vec<_> = signed_events_since_last_call()
				.into_iter()
				.filter(|e| matches!(e, SignedEvent::Slashed(..)))
				.collect();
			assert_eq!(slashed_events.len(), 1, "expected exactly one Slashed event");

			let ti_after = Balances::total_issuance();
			let target_after = Balances::free_balance(POT);

			assert_eq!(ti_before, ti_after, "total issuance must not change when slash is routed");
			assert!(target_after > target_before, "slash target balance must increase");
		});
	}
}
