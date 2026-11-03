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

use super::*;
use crate::session_rotation::{EraElectionPlanner, Eras};
use frame_support::assert_ok;
use sp_npos_elections::Support;
use substrate_test_utils::assert_eq_uvec;

use crate::tests::session_mock::ReceivedValidatorSets;

#[test]
fn planning_era_offset_less_0() {
	// same as `basic_setup_sessions_per_era`, but notice how `PagedElectionProceeded` happens
	// one session later, and planning era is incremented one session later
	ExtBuilder::default()
		.session_per_era(6)
		.planning_era_offset(0)
		.no_flush_events()
		.build_and_execute(|| {
			// this essentially makes the session duration 8. After 6 sessions we realize we have do
			// start an election (since offset = 0), then it is queued for one session (7), and then
			// activated (8).
			assert_eq!(Session::current_index(), 8);
			assert_eq!(active_era(), 1);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 3, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 4, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 5, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 6, active_era: 0, planned_era: 1 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 7, active_era: 0, planned_era: 1 },
					Event::EraPaid { era_index: 0, validator_payout: 20000, remainder: 0 },
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 }
				]
			);

			Session::roll_until_active_era(2);
			assert_eq!(Session::current_index(), 16);
			assert_eq!(active_era(), 2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 9, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 10, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 11, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 12, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 13, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 14, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 15, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 20000, remainder: 0 },
					Event::SessionRotated { starting_session: 16, active_era: 2, planned_era: 2 }
				]
			);
		});
}

#[test]
fn planning_era_offset_works_1() {
	// same as `basic_setup_sessions_per_era`, but notice how `PagedElectionProceeded` happens
	// one session later, and planning era is incremented one session later
	ExtBuilder::default()
		.session_per_era(6)
		.planning_era_offset(1)
		.no_flush_events()
		.build_and_execute(|| {
			// this essentially makes the session duration 7. After 5 sessions we realize we have do
			// start an election (since offset = 1), then it is queued for one session (6), and then
			// activated (7).
			assert_eq!(Session::current_index(), 7);
			assert_eq!(active_era(), 1);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 3, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 4, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 5, active_era: 0, planned_era: 1 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 6, active_era: 0, planned_era: 1 },
					Event::EraPaid { era_index: 0, validator_payout: 17500, remainder: 0 },
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 1 }
				]
			);

			Session::roll_until_active_era(2);
			assert_eq!(Session::current_index(), 14);
			assert_eq!(active_era(), 2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 9, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 10, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 11, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 12, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 13, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 17500, remainder: 0 },
					Event::SessionRotated { starting_session: 14, active_era: 2, planned_era: 2 }
				]
			);
		});
}

#[test]
fn planning_era_offset_works_2() {
	ExtBuilder::default()
		.session_per_era(6)
		.planning_era_offset(2)
		.no_flush_events()
		.build_and_execute(|| {
			// start election at 4, and send it over. Buffered at 6, activated at 6. This is the
			// expected behavior, and the default in `mock.rs`.
			assert_eq!(Session::current_index(), 6);
			assert_eq!(active_era(), 1);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 3, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 4, active_era: 0, planned_era: 1 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 5, active_era: 0, planned_era: 1 },
					Event::EraPaid { era_index: 0, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 6, active_era: 1, planned_era: 1 }
				]
			);

			Session::roll_until_active_era(2);
			assert_eq!(Session::current_index(), 12);
			assert_eq!(active_era(), 2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 9, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 10, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 11, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 12, active_era: 2, planned_era: 2 }
				]
			);
		});
}

#[test]
fn planning_era_offset_works_smart() {
	ExtBuilder::default()
		.session_per_era(6)
		.smart_era_planner()
		.no_flush_events()
		.build_and_execute(|| {
			// This works exactly the same as offset = 2, which means we send and rotate validators
			// such that the era duration remains 6 sessions.
			assert_eq!(Session::current_index(), 6);
			assert_eq!(active_era(), 1);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 3, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 4, active_era: 0, planned_era: 1 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 5, active_era: 0, planned_era: 1 },
					Event::EraPaid { era_index: 0, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 6, active_era: 1, planned_era: 1 }
				]
			);

			Session::roll_until_active_era(2);
			assert_eq!(Session::current_index(), 12);
			assert_eq!(active_era(), 2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 9, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 10, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 11, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 12, active_era: 2, planned_era: 2 }
				]
			);
		});
}

#[test]
fn planning_era_offset_works_smart_with_delay() {
	ExtBuilder::default()
		.session_per_era(6)
		.election_delay(7)
		.smart_era_planner()
		.no_flush_events()
		.build_and_execute(|| {
			// Same as above, but now election takes more time, more than 1 session to be exact.
			// Notice how the era duration is kept at 6.
			assert_eq!(Session::current_index(), 6);
			assert_eq!(active_era(), 1);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 1, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 2, active_era: 0, planned_era: 0 },
					Event::SessionRotated { starting_session: 3, active_era: 0, planned_era: 1 },
					Event::SessionRotated { starting_session: 4, active_era: 0, planned_era: 1 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 5, active_era: 0, planned_era: 1 },
					Event::EraPaid { era_index: 0, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 6, active_era: 1, planned_era: 1 }
				]
			);

			Session::roll_until_active_era(2);
			assert_eq!(Session::current_index(), 12);
			assert_eq!(active_era(), 2);

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					Event::SessionRotated { starting_session: 7, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 8, active_era: 1, planned_era: 1 },
					Event::SessionRotated { starting_session: 9, active_era: 1, planned_era: 2 },
					Event::SessionRotated { starting_session: 10, active_era: 1, planned_era: 2 },
					Event::PagedElectionProceeded { page: 0, result: Ok(2) },
					Event::SessionRotated { starting_session: 11, active_era: 1, planned_era: 2 },
					Event::EraPaid { era_index: 1, validator_payout: 15000, remainder: 0 },
					Event::SessionRotated { starting_session: 12, active_era: 2, planned_era: 2 }
				]
			);
		});
}

#[test]
fn new_era_elects_correct_number_of_validators() {
	ExtBuilder::default().nominate(true).validator_count(1).build_and_execute(|| {
		assert_eq!(ValidatorCount::<Test>::get(), 1);
		assert_eq!(session_validators().len(), 1);
	})
}

#[test]
fn less_than_needed_candidates_works() {
	ExtBuilder::default().validator_count(4).nominate(false).build_and_execute(|| {
		assert_eq_uvec!(Session::validators(), vec![31, 21, 11]);
		Session::roll_until_active_era(2);

		// Previous set is selected.
		assert_eq_uvec!(Session::validators(), vec![31, 21, 11]);

		// Only has self votes.
		assert!(ErasStakersPaged::<T>::iter_prefix_values((active_era(),))
			.all(|exposure| exposure.others.is_empty()));
	});
}

mod paged_exposures {
	use crate::session_rotation::Rotator;

	use super::*;

	#[test]
	fn can_page_exposure() {
		let mut others: Vec<IndividualExposure<AccountId, Balance>> = vec![];
		let mut total_stake: Balance = 0;
		// 19 nominators
		for i in 1..20 {
			let individual_stake: Balance = 100 * i as Balance;
			others.push(IndividualExposure { who: i, value: individual_stake });
			total_stake += individual_stake;
		}
		let own_stake: Balance = 500;
		total_stake += own_stake;
		assert_eq!(total_stake, 19_500);
		// build full exposure set
		let exposure: Exposure<AccountId, Balance> =
			Exposure { total: total_stake, own: own_stake, others };

		// when
		let (exposure_metadata, exposure_page): (
			PagedExposureMetadata<Balance>,
			Vec<ExposurePage<AccountId, Balance>>,
		) = exposure.clone().into_pages(3);

		// then
		// 7 pages of nominators.
		assert_eq!(exposure_page.len(), 7);
		assert_eq!(exposure_metadata.page_count, 7);
		// first page stake = 100 + 200 + 300
		assert!(matches!(exposure_page[0], ExposurePage { page_total: 600, .. }));
		// second page stake = 0 + 400 + 500 + 600
		assert!(matches!(exposure_page[1], ExposurePage { page_total: 1500, .. }));
		// verify overview has the total
		assert_eq!(exposure_metadata.total, 19_500);
		// verify total stake is same as in the original exposure.
		assert_eq!(
			exposure_page.iter().map(|a| a.page_total).reduce(|a, b| a + b).unwrap(),
			19_500 - exposure_metadata.own
		);
		// verify own stake is correct
		assert_eq!(exposure_metadata.own, 500);
		// verify number of nominators are same as in the original exposure.
		assert_eq!(exposure_page.iter().map(|a| a.others.len()).reduce(|a, b| a + b).unwrap(), 19);
		assert_eq!(exposure_metadata.nominator_count, 19);
	}

	fn exposure_pages(era: u32, who: AccountId) -> Vec<(u32, (Balance, u32))> {
		let mut res = ErasStakersPaged::<T>::iter_prefix((era, who))
			.map(|(page, expo)| (page, (expo.page_total, expo.others.len() as u32)))
			.collect::<Vec<_>>();
		res.sort_by_key(|(page, _)| *page);
		res
	}

	#[test]
	fn store_stakers_info_elect_works_page_size_1() {
		// scenario:
		// 2 page election. 5 validators involved.
		// Validator 1 has own-stake in first page, other-stake in both pages.
		// Validator 2 has own-stake in second page, other-stake in both pages
		// Validator 3 has no own-stake, other-stake in both pages
		// Validator 4 has no own-stake, other-stake in second page only
		// Validator 5 has no own-stake, other-stake in first page only
		// all other stakes are 250, and the same 101 account as it makes no difference.
		ExtBuilder::default().exposures_page_size(1).build_and_execute(|| {
			// roll 1 session such that we are in "planning more"
			Session::roll_to_next_session();
			assert_eq!(Rotator::<T>::planned_era(), 2);
			assert_eq!(Rotator::<T>::active_era(), 1);
			assert_eq!(ErasTotalStake::<T>::get(Rotator::<T>::planned_era()), 0);
			assert_eq!(
				ErasStakersOverview::<T>::iter_prefix(Rotator::<T>::planned_era()).count(),
				0
			);
			assert_eq!(
				ErasStakersPaged::<T>::iter_prefix((Rotator::<T>::planned_era(),)).count(),
				0
			);

			// stuff that goes into first page
			let exposures_page1 = bounded_vec![
				// validator 1, own-stake and other stake
				(
					1,
					Exposure::<AccountId, Balance> {
						total: 1000 + 250,
						own: 1000,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 2, other stake only
				(
					2,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 3, part of other stake only
				(
					3,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 4, nothing
				// validator 5, part of other stake only
				(
					5,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
			];

			// stuff that goes into second page
			let exposure_page2: BoundedExposuresOf<Test> = bounded_vec![
				// validator 1, other stake only
				(
					1,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 2, own-stake and other stake
				(
					2,
					Exposure::<AccountId, Balance> {
						total: 1000 + 250,
						own: 1000,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 3, part of other stake only
				(
					3,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 4, part of other stake only
				(
					4,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 101, value: 250 }]
					}
				),
				// validator 5, nothing
			];

			// our exposures are stored for this era.
			let current_era = current_era();
			assert_eq!(ErasTotalStake::<T>::get(current_era), 0);

			// insert page 1 of exposures
			assert_eq!(
				EraElectionPlanner::<T>::store_stakers_info(exposures_page1, current_era).to_vec(),
				vec![1, 2, 3, 5]
			);

			// overviews after inserting first page of exposures.
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &1).unwrap(),
				PagedExposureMetadata { total: 1250, own: 1000, nominator_count: 1, page_count: 1 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &2).unwrap(),
				PagedExposureMetadata { total: 250, own: 0, nominator_count: 1, page_count: 1 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &3).unwrap(),
				PagedExposureMetadata { total: 250, own: 0, nominator_count: 1, page_count: 1 },
			);
			assert!(ErasStakersOverview::<T>::get(current_era, &4).is_none());
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &5).unwrap(),
				PagedExposureMetadata { total: 250, own: 0, nominator_count: 1, page_count: 1 },
			);

			// total stake after first page of exposures.
			assert_eq!(ErasTotalStake::<T>::get(current_era), 2000);

			// details after first page of exposures.
			assert_eq!(exposure_pages(current_era, 1), vec![(0, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 2), vec![(0, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 3), vec![(0, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 4), vec![]);
			assert_eq!(exposure_pages(current_era, 5), vec![(0, (250, 1))]);

			// insert page 2 of exposures
			assert_eq!(
				EraElectionPlanner::<T>::store_stakers_info(exposure_page2, current_era).to_vec(),
				vec![1, 2, 3, 4]
			);

			// overviews after inserting second page of exposures.
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &1).unwrap(),
				PagedExposureMetadata { total: 1500, own: 1000, nominator_count: 2, page_count: 2 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &2).unwrap(),
				PagedExposureMetadata { total: 1500, own: 1000, nominator_count: 2, page_count: 2 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &3).unwrap(),
				PagedExposureMetadata { total: 500, own: 0, nominator_count: 2, page_count: 2 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &4).unwrap(),
				PagedExposureMetadata { total: 250, own: 0, nominator_count: 1, page_count: 1 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &5).unwrap(),
				PagedExposureMetadata { total: 250, own: 0, nominator_count: 1, page_count: 1 },
			);

			// total stake after second page of exposures.
			assert_eq!(ErasTotalStake::<T>::get(current_era), 4000);

			// details after second page of exposures.
			assert_eq!(exposure_pages(current_era, 1), vec![(0, (250, 1)), (1, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 2), vec![(0, (250, 1)), (1, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 3), vec![(0, (250, 1)), (1, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 4), vec![(0, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 5), vec![(0, (250, 1))]);
		})
	}

	#[test]
	fn store_stakers_info_elect_works_page_size_2() {
		// scenario:
		// 2 page election. 5 validators involved.
		// Validator 1 has own-stake in first page, other-stake in both pages (3 + 2).
		// Validator 2 has own-stake in second page, other-stake in both pages (2 + 3)
		// Validator 3 has no own-stake, other-stake in both pages (3 + 1)
		// Validator 4 has no own-stake, other-stake in second page only (0 + 3)
		// Validator 5 has no own-stake, other-stake in first page only (2 + 0)
		// all other stakes are 250, and the same 101 (or more) account as it makes no difference.
		ExtBuilder::default().exposures_page_size(2).build_and_execute(|| {
			// roll 1 session such that we are in "planning more"
			Session::roll_to_next_session();
			assert_eq!(Rotator::<T>::planned_era(), 2);
			assert_eq!(Rotator::<T>::active_era(), 1);
			assert_eq!(ErasTotalStake::<T>::get(Rotator::<T>::planned_era()), 0);
			assert_eq!(
				ErasStakersOverview::<T>::iter_prefix(Rotator::<T>::planned_era()).count(),
				0
			);
			assert_eq!(
				ErasStakersPaged::<T>::iter_prefix((Rotator::<T>::planned_era(),)).count(),
				0
			);

			// stuff that goes into first page
			let exposures_page1 = bounded_vec![
				// validator 1, own-stake and other stake
				(
					1,
					Exposure::<AccountId, Balance> {
						total: 1000 + 750,
						own: 1000,
						others: vec![
							IndividualExposure { who: 101, value: 250 },
							IndividualExposure { who: 102, value: 250 },
							IndividualExposure { who: 103, value: 250 },
						]
					}
				),
				// validator 2, other stake only
				(
					2,
					Exposure::<AccountId, Balance> {
						total: 500,
						own: 0,
						others: vec![
							IndividualExposure { who: 101, value: 250 },
							IndividualExposure { who: 102, value: 250 },
						]
					}
				),
				// validator 3, part of other stake only
				(
					3,
					Exposure::<AccountId, Balance> {
						total: 750,
						own: 0,
						others: vec![
							IndividualExposure { who: 101, value: 250 },
							IndividualExposure { who: 102, value: 250 },
							IndividualExposure { who: 103, value: 250 }
						]
					}
				),
				// validator 4, nothing
				// validator 5, all of other stake only
				(
					5,
					Exposure::<AccountId, Balance> {
						total: 500,
						own: 0,
						others: vec![
							IndividualExposure { who: 101, value: 250 },
							IndividualExposure { who: 102, value: 250 },
						]
					}
				),
			];

			let exposure_page2: BoundedExposuresOf<Test> = bounded_vec![
				// validator 1, other stake only
				(
					1,
					Exposure::<AccountId, Balance> {
						total: 500,
						own: 0,
						others: vec![
							IndividualExposure { who: 104, value: 250 },
							IndividualExposure { who: 105, value: 250 }
						]
					}
				),
				// validator 2, own-stake and other stake
				(
					2,
					Exposure::<AccountId, Balance> {
						total: 1000 + 750,
						own: 1000,
						others: vec![
							IndividualExposure { who: 103, value: 250 },
							IndividualExposure { who: 104, value: 250 },
							IndividualExposure { who: 105, value: 250 },
						]
					}
				),
				// validator 3, part of other stake only
				(
					3,
					Exposure::<AccountId, Balance> {
						total: 250,
						own: 0,
						others: vec![IndividualExposure { who: 103, value: 250 },]
					}
				),
				// validator 4, all of other stake only
				(
					4,
					Exposure::<AccountId, Balance> {
						total: 750,
						own: 0,
						others: vec![
							IndividualExposure { who: 101, value: 250 },
							IndividualExposure { who: 102, value: 250 },
							IndividualExposure { who: 103, value: 250 }
						]
					}
				),
				// validator 5, nothing
			];

			// our exposures are stored for this era.
			let current_era = current_era();

			// insert page 1 of exposures
			assert_eq!(
				EraElectionPlanner::<T>::store_stakers_info(exposures_page1, current_era).to_vec(),
				vec![1, 2, 3, 5]
			);

			// overviews after inserting first page of exposures.
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &1).unwrap(),
				PagedExposureMetadata { total: 1750, own: 1000, nominator_count: 3, page_count: 2 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &2).unwrap(),
				PagedExposureMetadata { total: 500, own: 0, nominator_count: 2, page_count: 1 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &3).unwrap(),
				PagedExposureMetadata { total: 750, own: 0, nominator_count: 3, page_count: 2 },
			);
			assert!(ErasStakersOverview::<T>::get(current_era, &4).is_none());
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &5).unwrap(),
				PagedExposureMetadata { total: 500, own: 0, nominator_count: 2, page_count: 1 },
			);

			// total stake after first page of exposures.
			assert_eq!(ErasTotalStake::<T>::get(current_era), 3500);

			// details after first page of exposures.
			assert_eq!(exposure_pages(current_era, 1), vec![(0, (500, 2)), (1, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 2), vec![(0, (500, 2))]);
			assert_eq!(exposure_pages(current_era, 3), vec![(0, (500, 2)), (1, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 4), vec![]);
			assert_eq!(exposure_pages(current_era, 5), vec![(0, (500, 2))]);

			// insert page 2 of exposures
			assert_eq!(
				EraElectionPlanner::<T>::store_stakers_info(exposure_page2, current_era).to_vec(),
				vec![1, 2, 3, 4]
			);

			// overviews after inserting second page of exposures.
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &1).unwrap(),
				PagedExposureMetadata { total: 2250, own: 1000, nominator_count: 5, page_count: 3 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &2).unwrap(),
				PagedExposureMetadata { total: 2250, own: 1000, nominator_count: 5, page_count: 3 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &3).unwrap(),
				PagedExposureMetadata { total: 1000, own: 0, nominator_count: 4, page_count: 2 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &4).unwrap(),
				PagedExposureMetadata { total: 750, own: 0, nominator_count: 3, page_count: 2 },
			);
			assert_eq!(
				ErasStakersOverview::<T>::get(current_era, &5).unwrap(),
				PagedExposureMetadata { total: 500, own: 0, nominator_count: 2, page_count: 1 },
			);

			// total stake after second page of exposures.
			assert_eq!(ErasTotalStake::<T>::get(current_era), 6750);

			// details after second page of exposures.
			assert_eq!(
				exposure_pages(current_era, 1),
				vec![(0, (500, 2)), (1, (500, 2)), (2, (250, 1))]
			);
			assert_eq!(
				exposure_pages(current_era, 2),
				vec![(0, (500, 2)), (1, (500, 2)), (2, (250, 1))]
			);
			assert_eq!(exposure_pages(current_era, 3), vec![(0, (500, 2)), (1, (500, 2))]);
			assert_eq!(exposure_pages(current_era, 4), vec![(0, (500, 2)), (1, (250, 1))]);
			assert_eq!(exposure_pages(current_era, 5), vec![(0, (500, 2))]);
		})
	}
}

mod electable_stashes {
	use super::*;

	#[test]
	fn add_electable_stashes_work() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			MaxValidatorSet::set(5);
			assert_eq!(MaxValidatorSet::get(), 5);
			assert!(ElectableStashes::<Test>::get().is_empty());

			// adds stashes without duplicates, do not overflow bounds.
			assert_ok!(EraElectionPlanner::<T>::add_electables(vec![1u64, 2, 3].into_iter()));
			assert_eq!(
				ElectableStashes::<Test>::get().into_inner().into_iter().collect::<Vec<_>>(),
				vec![1, 2, 3]
			);

			// adds with duplicates which are deduplicated implicitly, no not overflow bounds.
			assert_ok!(EraElectionPlanner::<T>::add_electables(vec![1u64, 2, 4].into_iter()));
			assert_eq!(
				ElectableStashes::<Test>::get().into_inner().into_iter().collect::<Vec<_>>(),
				vec![1, 2, 3, 4]
			);
		})
	}

	#[test]
	fn add_electable_stashes_overflow_works() {
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			MaxValidatorSet::set(5);
			assert_eq!(MaxValidatorSet::get(), 5);
			assert!(ElectableStashes::<Test>::get().is_empty());

			// adds stashes so that bounds are overflown, fails and internal state changes so that
			// all slots are filled. error will return the idx of the first account that was not
			// included.
			let expected_idx_not_included = 5; // stash 6.
			assert_eq!(
				EraElectionPlanner::<T>::add_electables(
					vec![1u64, 2, 3, 4, 5, 6, 7, 8].into_iter()
				),
				Err(expected_idx_not_included)
			);
			// the included were added to the electable stashes, despite the error.
			assert_eq!(
				ElectableStashes::<Test>::get().into_inner().into_iter().collect::<Vec<_>>(),
				vec![1, 2, 3, 4, 5]
			);
		})
	}

	#[test]
	fn overflow_electable_stashes_no_exposures_work() {
		// ensures exposures are stored only for the electable stashes that fit within the
		// electable stashes bounds in case of overflow.
		ExtBuilder::default().try_state(false).build_and_execute(|| {
			MaxValidatorSet::set(2);
			assert!(ElectableStashes::<Test>::get().is_empty());

			let supports = to_bounded_supports(vec![
				(1, Support { total: 100, voters: vec![(10, 1_000)] }),
				(2, Support { total: 200, voters: vec![(20, 2_000)] }),
				(3, Support { total: 300, voters: vec![(30, 3_000)] }),
				(4, Support { total: 400, voters: vec![(40, 4_000)] }),
			]);

			// error due to bounds.
			let expected_not_included = 2;
			assert_eq!(
				EraElectionPlanner::<T>::do_elect_paged_inner(supports),
				Err(expected_not_included)
			);

			// electable stashes have been collected to the max bounds despite the error.
			assert_eq!(ElectableStashes::<Test>::get().into_iter().collect::<Vec<_>>(), vec![1, 2]);

			let exposure_exists = |acc, era| Eras::<Test>::get_full_exposure(era, &acc).total != 0;

			// exposures were only collected for electable stashes in bounds (1 and 2).
			assert!(exposure_exists(1, 1));
			assert!(exposure_exists(2, 1));
			assert!(!exposure_exists(3, 1));
			assert!(!exposure_exists(4, 1));
		})
	}
}

mod paged_on_initialize_era_election_planner {
	use pallet_staking_async_rc_client::ValidatorSetReport;

	use super::*;

	#[test]
	fn single_page_election_works() {
		ExtBuilder::default()
			// set desired targets to 3.
			.validator_count(3)
			.build_and_execute(|| {
				// single page.
				let pages: BlockNumber = EraElectionPlanner::<T>::election_pages().into();
				assert_eq!(pages, 1);

				// we will start the next election at the start of block 20
				assert_eq!(System::block_number(), 15);
				assert_eq!(PlanningEraOffset::get(), 2);

				// genesis validators are now in place.
				assert_eq!(current_era(), 1);
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31]);

				// force unstake of 31 to ensure the election results of the next era are
				// different than genesis.
				assert_ok!(Staking::force_unstake(RuntimeOrigin::root(), 31, 0));

				//  use all registered validators as potential targets.
				let expected_elected = vec![11, 21];
				ValidatorCount::<Test>::set(expected_elected.len() as u32);

				// 1. start signal is sent, election result will come next block.
				Session::roll_until(20);
				assert_eq!(NextElectionPage::<Test>::get(), None);
				assert!(ElectableStashes::<Test>::get().is_empty());
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Waiting);

				// 2. starts preparing election at the (election_prediction - n_pages) block.
				Session::roll_next();

				// electing started, but since single-page, we don't set `NextElectionPage` at all.
				assert_eq!(NextElectionPage::<Test>::get(), None);
				assert!(ElectableStashes::<Test>::get().is_empty());
				// Electable stashes are already drained and sent to RC client.
				assert_eq!(
					ReceivedValidatorSets::get_last(),
					ValidatorSetReport {
						id: 2,
						leftover: false,
						new_validator_set: vec![11, 21],
						prune_up_to: None
					}
				);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Waiting);

				assert_eq!(current_era(), 2);
				assert_eq!(active_era(), 1);

				// check old exposures
				assert_eq_uvec!(
					era_exposures(1),
					vec![
						(
							11,
							Exposure {
								total: 1250,
								own: 1000 as Balance,
								others: vec![IndividualExposure { who: 101, value: 250 }]
							}
						),
						(
							21,
							Exposure {
								total: 1250,
								own: 1000 as Balance,
								others: vec![IndividualExposure { who: 101, value: 250 }]
							}
						),
						(31, Exposure { total: 500, own: 500 as Balance, others: vec![] }),
					]
				);

				// check new exposures
				assert_eq_uvec!(
					era_exposures(2),
					vec![
						(
							11,
							Exposure {
								total: 1250,
								own: 1000 as Balance,
								others: vec![IndividualExposure { who: 101, value: 250 }]
							}
						),
						(
							21,
							Exposure {
								total: 1250,
								own: 1000 as Balance,
								others: vec![IndividualExposure { who: 101, value: 250 }]
							}
						),
					]
				);

				// era progressed and electable stashes have been served to session pallet.
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31]);

				// 4. in the next era, the validator set does not include 31 anymore which was
				// unstaked.
				Session::roll_until_active_era(2);

				assert_eq_uvec!(Session::validators(), vec![11, 21]);
			})
	}

	#[test]
	fn multi_page_election_works() {
		ExtBuilder::default()
			.add_staker(61, 1000, StakerStatus::Validator)
			.add_staker(71, 1000, StakerStatus::Validator)
			.add_staker(81, 1000, StakerStatus::Validator)
			.add_staker(91, 1000, StakerStatus::Validator)
			.multi_page_election_provider(3)
			.validator_count(6)
			.election_bounds(3, 10)
			.build_and_execute(|| {
				type A = AccountId;
				type B = Balance;
				// NOTE: we cannot really enforce MaxBackersPerWinner and ValidatorCount here as our
				// election provider in the mock is rather dumb and cannot respect them atm.

				// we will start the next election at the start of block 20
				assert_eq!(System::block_number(), 15);
				assert_eq!(PlanningEraOffset::get(), 2);

				// 1. election signal is sent here,
				Session::roll_until(20);
				assert_eq!(
					staking_events_since_last_call(),
					vec![Event::SessionRotated {
						starting_session: 4,
						active_era: 1,
						planned_era: 2
					}]
				);

				assert_eq!(NextElectionPage::<Test>::get(), None);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Waiting);
				assert!(ElectableStashes::<Test>::get().is_empty());

				// page 2 fetched, next is 1
				Session::roll_until(21);
				assert_eq!(NextElectionPage::<Test>::get(), Some(1));
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Ongoing(31));
				assert_eq!(
					ElectableStashes::<Test>::get().into_iter().collect::<Vec<AccountId>>(),
					vec![11, 21, 31]
				);

				assert_eq_uvec!(
					era_exposures(2),
					vec![
						(11, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(21, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(31, Exposure::<A, B> { total: 500, own: 500, others: vec![] }),
					]
				);

				// page 1, next is 0
				Session::roll_until(22);
				// the electable stashes remain the same.
				assert_eq_uvec!(
					ElectableStashes::<Test>::get().into_iter().collect::<Vec<_>>(),
					vec![11, 21, 31, 61, 71, 81]
				);
				assert_eq!(NextElectionPage::<Test>::get(), Some(0));
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Ongoing(81));

				assert_eq_uvec!(
					era_exposures(2),
					vec![
						(31, Exposure::<A, B> { total: 500, own: 500, others: vec![] }),
						(21, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(81, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(71, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(11, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(61, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] })
					]
				);

				// fetch 0, done.
				Session::roll_until(23);
				// the electable stashes are now empty
				assert!(ElectableStashes::<Test>::get().is_empty());
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Waiting);
				assert_eq!(NextElectionPage::<Test>::get(), None);

				// check exposures
				assert_eq_uvec!(
					era_exposures(2),
					vec![
						(31, Exposure::<A, B> { total: 500, own: 500, others: vec![] }),
						(
							21,
							Exposure::<A, B> {
								total: 1250,
								own: 1000,
								others: vec![IndividualExposure { who: 101, value: 250 }]
							}
						),
						(81, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(71, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(91, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] }),
						(
							11,
							Exposure::<A, B> {
								total: 1250,
								own: 1000,
								others: vec![IndividualExposure { who: 101, value: 250 }]
							}
						),
						(61, Exposure::<A, B> { total: 1000, own: 1000, others: vec![] })
					]
				);

				// and are sent
				assert_eq!(
					ReceivedValidatorSets::get_last(),
					ValidatorSetReport {
						id: 2,
						leftover: false,
						new_validator_set: vec![11, 21, 31, 61, 71, 81, 91],
						prune_up_to: None
					}
				);

				assert_eq!(NextElectionPage::<Test>::get(), None);
				assert_eq!(
					staking_events_since_last_call(),
					vec![
						Event::PagedElectionProceeded { page: 2, result: Ok(3) },
						Event::PagedElectionProceeded { page: 1, result: Ok(3) },
						Event::PagedElectionProceeded { page: 0, result: Ok(1) }
					]
				);

				// go to activation of this validator set.
				Session::roll_until_active_era(2);

				// the new era validators are the expected elected stashes.
				assert_eq_uvec!(Session::validators(), vec![11, 21, 31, 61, 71, 81, 91]);
			})
	}

	// #[test]
	// fn multi_page_exposure_and_multi_page_elect() {
	// 	todo!("an election with 3 pages, with 4 backers per exposures, which are stored in
	// MaxExposurePageSize = 6, ergo transformed into 2 pages of final exposure") }

	// #[test]
	// fn multi_page_election_with_mulit_page_exposures_rewards_work() {
	// 	ExtBuilder::default()
	// 		.add_staker(61, 61, 1000, StakerStatus::Validator)
	// 		.add_staker(71, 71, 1000, StakerStatus::Validator)
	//         .add_staker(1, 1, 5, StakerStatus::Nominator(vec![21, 31, 71]))
	//         .add_staker(2, 2, 5, StakerStatus::Nominator(vec![21, 31, 71]))
	//         .add_staker(3, 3, 5, StakerStatus::Nominator(vec![21, 31, 71]))
	// 		.multi_page_election_provider(3)
	//         .max_winners_per_page(3)
	//         .exposures_page_size(2)
	// 		.build_and_execute(|| {
	// 			// election provider has 3 pages.
	// 			let pages: BlockNumber =
	// 				<<Test as Config>::ElectionProvider as ElectionProvider>::Pages::get().into();
	// 			assert_eq!(pages, 3);
	//             // 3 max winners per page.
	//             let max_winners_page = <<Test as Config>::ElectionProvider as
	// ElectionProvider>::MaxWinnersPerPage::get();             assert_eq!(max_winners_page, 3);

	//     		// setup validator payee prefs and 10% commission.
	//             for s in vec![21, 31, 71] {
	//     		    Payee::<Test>::insert(s, RewardDestination::Account(s));
	//                 let prefs = ValidatorPrefs { commission: Perbill::from_percent(10),
	// ..Default::default() }; 		        Validators::<Test>::insert(s, prefs.clone());
	//             }

	//             let init_balance_all = vec![21, 31, 71, 1, 2, 3].iter().fold(0, |mut acc, s| {
	//                 acc += asset::total_balance::<Test>(&s);
	//                 acc
	//             });

	//             // progress era.
	// 			assert_eq!(current_era(), 0);
	//             start_active_era(1);
	// 			assert_eq!(current_era(), 1);
	//             assert_eq!(Session::validators(), vec![21, 31, 71]);

	//             // distribute reward,
	// 	        Pallet::<Test>::reward_by_ids(vec![(21, 50)]);
	// 	        Pallet::<Test>::reward_by_ids(vec![(31, 50)]);
	// 	        Pallet::<Test>::reward_by_ids(vec![(71, 50)]);

	//     		let total_payout = validator_payout_for(time_per_era());

	//             start_active_era(2);

	//             // all the validators exposed in era 1 have two pages of exposures, since
	// exposure             // page size is 2.
	//             assert_eq!(MaxExposurePageSize::get(), 2);
	//             assert_eq!(Eras::<Test>::exposure_page_count(1, &21), 2);
	//             assert_eq!(Eras::<Test>::exposure_page_count(1, &31), 2);
	//             assert_eq!(Eras::<Test>::exposure_page_count(1, &71), 2);

	//             make_all_reward_payment(1);

	//             let balance_all = vec![21, 31, 71, 1, 2, 3].iter().fold(0, |mut acc, s| {
	//                 acc += asset::total_balance::<Test>(&s);
	//                 acc
	//             });

	// 		    assert_eq_error_rate!(
	//                 total_payout,
	//                 balance_all - init_balance_all,
	//                 4
	//             );
	//         })
	// }

	// #[test]
	// fn multi_page_election_is_graceful() {
	// 	// demonstrate that in a multi-page election, in some of the `elect(_)` calls fail we won't
	// 	// bail right away.
	// 	ExtBuilder::default().multi_page_election_provider(3).build_and_execute(|| {
	// 		// load some exact data into the election provider, some of which are error or empty.
	// 		let correct_results = <Test as Config>::GenesisElectionProvider::elect(0);
	// 		CustomElectionSupports::set(Some(vec![
	// 			// page 0.
	// 			correct_results.clone(),
	// 			// page 1.
	// 			Err(onchain::Error::FailedToBound),
	// 			// page 2.
	// 			Ok(Default::default()),
	// 		]));

	// 		// genesis era.
	// 		assert_eq!(current_era(), 0);

	// 		let next_election =
	// 			<Staking as ElectionDataProvider>::next_election_prediction(System::block_number());
	// 		assert_eq!(next_election, 10);

	// 		// try-state sanity check.
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// 1. election prep hasn't started yet, election cursor and electable stashes are
	// 		// not set yet.
	// 		roll_to_block(6);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));
	// 		assert_eq!(NextElectionPage::<Test>::get(), None);
	// 		assert!(ElectableStashes::<Test>::get().is_empty());

	// 		// 2. starts preparing election at the (election_prediction - n_pages) block.
	// 		//  fetches lsp (i.e. 2).
	// 		roll_to_block(7);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// electing started at cursor is set once the election starts to be prepared.
	// 		assert_eq!(NextElectionPage::<Test>::get(), Some(1));
	// 		// in elect(2) we won't collect any stashes yet.
	// 		assert!(ElectableStashes::<Test>::get().is_empty());

	// 		// 3. progress one block to fetch page 1.
	// 		roll_to_block(8);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// in elect(1) we won't collect any stashes yet.
	// 		assert!(ElectableStashes::<Test>::get().is_empty());
	// 		// election cursor is updated
	// 		assert_eq!(NextElectionPage::<Test>::get(), Some(0));

	// 		// 4. progress one block to fetch mps (i.e. 0).
	// 		roll_to_block(9);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// some stashes come in.
	// 		assert_eq!(
	// 			ElectableStashes::<Test>::get().into_iter().collect::<Vec<_>>(),
	// 			vec![11 as AccountId, 21]
	// 		);
	// 		// cursor is now none
	// 		assert_eq!(NextElectionPage::<Test>::get(), None);

	// 		// events thus far
	// 		assert_eq!(
	// 			staking_events_since_last_call(),
	// 			vec![
	// 				Event::PagedElectionProceeded { page: 2, result: Ok(0) },
	// 				Event::PagedElectionProceeded { page: 1, result: Err(0) },
	// 				Event::PagedElectionProceeded { page: 0, result: Ok(2) }
	// 			]
	// 		);

	// 		// upon fetching page 0, the electing started will remain in storage until the
	// 		// era rotates.
	// 		assert_eq!(current_era(), 0);

	// 		// Next block the era will rotate.
	// 		roll_to_block(10);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// and all the metadata has been cleared up and ready for the next election.
	// 		assert!(NextElectionPage::<Test>::get().is_none());
	// 		assert!(ElectableStashes::<Test>::get().is_empty());

	// 		// and the overall staking worked fine.
	// 		assert_eq!(staking_events_since_last_call(), vec![Event::StakersElected]);
	// 	})
	// }

	// #[test]
	// fn multi_page_election_fails_if_not_enough_validators() {
	// 	// a graceful multi-page election still fails if not enough validators are provided.
	// 	ExtBuilder::default().multi_page_election_provider(3).build_and_execute(|| {
	// 		// load some exact data into the election provider, some of which are error or
	// 		// empty.
	// 		let correct_results = <Test as Config>::GenesisElectionProvider::elect(0);
	// 		CustomElectionSupports::set(Some(vec![
	// 			// page 0.
	// 			correct_results.clone(),
	// 			// page 1.
	// 			Err(onchain::Error::FailedToBound),
	// 			// page 2.
	// 			Ok(Default::default()),
	// 		]));

	// 		// genesis era.
	// 		assert_eq!(current_era(), 0);

	// 		let next_election =
	// 			<Staking as ElectionDataProvider>::next_election_prediction(System::block_number());
	// 		assert_eq!(next_election, 10);

	// 		// try-state sanity check.
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// 1. election prep hasn't started yet, election cursor and electable stashes are
	// 		// not set yet.
	// 		roll_to_block(6);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));
	// 		assert_eq!(NextElectionPage::<Test>::get(), None);
	// 		assert!(ElectableStashes::<Test>::get().is_empty());

	// 		// 2. starts preparing election at the (election_prediction - n_pages) block.
	// 		//  fetches lsp (i.e. 2).
	// 		roll_to_block(7);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// electing started at cursor is set once the election starts to be prepared.
	// 		assert_eq!(NextElectionPage::<Test>::get(), Some(1));
	// 		// in elect(2) we won't collect any stashes yet.
	// 		assert!(ElectableStashes::<Test>::get().is_empty());

	// 		// 3. progress one block to fetch page 1.
	// 		roll_to_block(8);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// in elect(1) we won't collect any stashes yet.
	// 		assert!(ElectableStashes::<Test>::get().is_empty());
	// 		// election cursor is updated
	// 		assert_eq!(NextElectionPage::<Test>::get(), Some(0));

	// 		// 4. progress one block to fetch mps (i.e. 0).
	// 		roll_to_block(9);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// some stashes come in.
	// 		assert_eq!(
	// 			ElectableStashes::<Test>::get().into_iter().collect::<Vec<_>>(),
	// 			vec![11 as AccountId, 21]
	// 		);
	// 		// cursor is now none
	// 		assert_eq!(NextElectionPage::<Test>::get(), None);

	// 		// events thus far
	// 		assert_eq!(
	// 			staking_events_since_last_call(),
	// 			vec![
	// 				Event::PagedElectionProceeded { page: 2, result: Ok(0) },
	// 				Event::PagedElectionProceeded { page: 1, result: Err(0) },
	// 				Event::PagedElectionProceeded { page: 0, result: Ok(2) }
	// 			]
	// 		);

	// 		// upon fetching page 0, the electing started will remain in storage until the
	// 		// era rotates.
	// 		assert_eq!(current_era(), 0);

	// 		// Next block the era will rotate.
	// 		roll_to_block(10);
	// 		assert_ok!(Staking::ensure_snapshot_metadata_state(System::block_number()));

	// 		// and all the metadata has been cleared up and ready for the next election.
	// 		assert!(NextElectionPage::<Test>::get().is_none());
	// 		assert!(ElectableStashes::<Test>::get().is_empty());

	// 		// and the overall staking worked fine.
	// 		assert_eq!(staking_events_since_last_call(), vec![Event::StakingElectionFailed]);
	// 	})
	// }
}
