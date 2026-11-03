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
use frame_election_provider_support::ElectionDataProvider;

#[test]
fn set_minimum_active_stake_is_correct() {
	ExtBuilder::default()
		.nominate(false)
		.add_staker(61, 2_000, StakerStatus::<AccountId>::Nominator(vec![21]))
		.add_staker(71, 10, StakerStatus::<AccountId>::Nominator(vec![21]))
		.add_staker(81, 50, StakerStatus::<AccountId>::Nominator(vec![21]))
		.build_and_execute(|| {
			// default bounds are unbounded.
			assert_ok!(<Staking as ElectionDataProvider>::electing_voters(
				DataProviderBounds::default(),
				0
			));
			assert_eq!(MinimumActiveStake::<Test>::get(), 10);

			// remove staker with lower bond by limiting the number of voters and check
			// `MinimumActiveStake` again after electing voters.
			let bounds = ElectionBoundsBuilder::default().voters_count(5.into()).build();
			assert_ok!(<Staking as ElectionDataProvider>::electing_voters(bounds.voters, 0));
			assert_eq!(MinimumActiveStake::<Test>::get(), 50);
		});
}

#[test]
fn set_minimum_active_stake_lower_bond_works() {
	// lower non-zero active stake below `MinNominatorBond` is the minimum active stake if
	// it is selected as part of the npos voters.
	ExtBuilder::default().has_stakers(true).nominate(true).build_and_execute(|| {
		// default `ExtBuilder` sets `MinNominatorBond = ED + 1 = 2`.
		assert_eq!(MinNominatorBond::<Test>::get(), 2);
		assert_eq!(<Test as Config>::VoterList::count(), 4);

		assert_ok!(Staking::bond(RuntimeOrigin::signed(4), 5, RewardDestination::Staked,));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(4), vec![11]));
		assert_eq!(<Test as Config>::VoterList::count(), 5);

		let voters_before =
			<Staking as ElectionDataProvider>::electing_voters(DataProviderBounds::default(), 0)
				.unwrap();
		assert_eq!(MinimumActiveStake::<Test>::get(), 5);

		// update minimum nominator bond.
		MinNominatorBond::<Test>::set(10);
		assert_eq!(MinNominatorBond::<Test>::get(), 10);
		// voter list still considers nominator 4 for voting, even though its active stake is
		// lower than `MinNominatorBond`.
		assert_eq!(<Test as Config>::VoterList::count(), 5);

		let voters =
			<Staking as ElectionDataProvider>::electing_voters(DataProviderBounds::default(), 0)
				.unwrap();
		assert_eq!(voters_before, voters);

		// minimum active stake is lower than `MinNominatorBond`.
		assert_eq!(MinimumActiveStake::<Test>::get(), 5);
	});
}

#[test]
fn set_minimum_active_bond_corrupt_state() {
	ExtBuilder::default()
		.has_stakers(true)
		.nominate(true)
		.add_staker(61, 2_000, StakerStatus::<AccountId>::Nominator(vec![21]))
		.build_and_execute(|| {
			assert_eq!(Staking::weight_of(&101), 500);
			let voters = <Staking as ElectionDataProvider>::electing_voters(
				DataProviderBounds::default(),
				0,
			)
			.unwrap();
			assert_eq!(voters.len(), 5);
			assert_eq!(MinimumActiveStake::<Test>::get(), 500);

			Session::roll_until_active_era(10);
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(101), 200));
			Session::roll_until_active_era(20);
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(101), 100));

			// corrupt ledger state by lowering max unlocking chunks bounds.
			MaxUnlockingChunks::set(1);

			let voters = <Staking as ElectionDataProvider>::electing_voters(
				DataProviderBounds::default(),
				0,
			)
			.unwrap();

			// number of returned voters decreases since ledger entry of stash 101 is now
			// corrupt.
			assert_eq!(voters.len(), 4);
			// minimum active stake does not take into consideration the corrupt entry.
			assert_eq!(MinimumActiveStake::<Test>::get(), 2_000);

			// voter weight of corrupted ledger entry is 0.
			assert_eq!(Staking::weight_of(&101), 0);

			// reset max unlocking chunks for try_state to pass.
			MaxUnlockingChunks::set(32);
		})
}

#[test]
fn voters_include_self_vote() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// default bounds are unbounded.
		assert!(<Validators<Test>>::iter().map(|(x, _)| x).all(|v| Staking::electing_voters(
			DataProviderBounds::default(),
			0
		)
		.unwrap()
		.into_iter()
		.any(|(w, _, t)| { v == w && t[0] == w })))
	})
}

#[test]
#[should_panic]
#[cfg(debug_assertions)]
fn only_iterates_max_2_times_max_allowed_len() {
	ExtBuilder::default()
		.nominate(false)
		// the best way to invalidate a bunch of nominators is to have them nominate a lot of
		// ppl, but then lower the MaxNomination limit.
		.add_staker(61, 2_000, StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]))
		.add_staker(71, 2_000, StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]))
		.add_staker(81, 2_000, StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]))
		.build_and_execute(|| {
			let bounds_builder = ElectionBoundsBuilder::default();
			// all voters ordered by stake,
			assert_eq!(
				<Test as Config>::VoterList::iter().collect::<Vec<_>>(),
				vec![61, 71, 81, 11, 21, 31]
			);

			AbsoluteMaxNominations::set(2);

			// we want 2 voters now, and in maximum we allow 4 iterations. This is what happens:
			// 61 is pruned;
			// 71 is pruned;
			// 81 is pruned;
			// 11 is taken;
			// we finish since the 2x limit is reached.
			assert_eq!(
				Staking::electing_voters(bounds_builder.voters_count(2.into()).build().voters, 0)
					.unwrap()
					.iter()
					.map(|(stash, _, _)| stash)
					.copied()
					.collect::<Vec<_>>(),
				vec![11],
			);
		});
}

#[test]
fn respects_snapshot_count_limits() {
	ExtBuilder::default()
		.set_status(41, StakerStatus::Validator)
		.build_and_execute(|| {
			// sum of all nominators who'd be voters (1), plus the self-votes (4).
			assert_eq!(<Test as Config>::VoterList::count(), 5);

			let bounds_builder = ElectionBoundsBuilder::default();

			// if voter count limit is less..
			assert_eq!(
				Staking::electing_voters(bounds_builder.voters_count(1.into()).build().voters, 0)
					.unwrap()
					.len(),
				1
			);

			// if voter count limit is equal..
			assert_eq!(
				Staking::electing_voters(bounds_builder.voters_count(5.into()).build().voters, 0)
					.unwrap()
					.len(),
				5
			);

			// if voter count limit is more.
			assert_eq!(
				Staking::electing_voters(bounds_builder.voters_count(55.into()).build().voters, 0)
					.unwrap()
					.len(),
				5
			);

			// if target count limit is more..
			assert_eq!(
				Staking::electable_targets(
					bounds_builder.targets_count(6.into()).build().targets,
					0,
				)
				.unwrap()
				.len(),
				4
			);

			// if target count limit is equal..
			assert_eq!(
				Staking::electable_targets(
					bounds_builder.targets_count(4.into()).build().targets,
					0,
				)
				.unwrap()
				.len(),
				4
			);

			// if target limit count is less, then we return an error.
			assert_eq!(
				Staking::electable_targets(
					bounds_builder.targets_count(1.into()).build().targets,
					0
				)
				.unwrap()
				.len(),
				1,
			);
		});
}

#[test]
fn respects_snapshot_size_limits() {
	ExtBuilder::default().build_and_execute(|| {
		// voters: set size bounds that allows only for 1 voter.
		let bounds = ElectionBoundsBuilder::default().voters_size(26.into()).build();
		let elected = Staking::electing_voters(bounds.voters, 0).unwrap();
		assert!(elected.encoded_size() == 26 as usize);
		let prev_len = elected.len();

		// larger size bounds means more quota for voters.
		let bounds = ElectionBoundsBuilder::default().voters_size(100.into()).build();
		let elected = Staking::electing_voters(bounds.voters, 0).unwrap();
		assert!(elected.encoded_size() <= 100 as usize);
		assert!(elected.len() > 1 && elected.len() > prev_len);

		// targets: set size bounds that allows for only one target to fit in the snapshot.
		let bounds = ElectionBoundsBuilder::default().targets_size(10.into()).build();
		let elected = Staking::electable_targets(bounds.targets, 0).unwrap();
		assert!(elected.encoded_size() == 9 as usize);
		let prev_len = elected.len();

		// larger size bounds means more space for targets.
		let bounds = ElectionBoundsBuilder::default().targets_size(100.into()).build();
		let elected = Staking::electable_targets(bounds.targets, 0).unwrap();
		assert!(elected.encoded_size() <= 100 as usize);
		assert!(elected.len() > 1 && elected.len() > prev_len);
	});
}

#[test]
fn nomination_quota_checks_at_nominate_works() {
	ExtBuilder::default().nominate(false).build_and_execute(|| {
		// stash bond of 222 has a nomination quota of 2 targets.
		bond(61, 222);
		assert_eq!(Staking::api_nominations_quota(222), 2);

		// nominating with targets below the nomination quota works.
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(61), vec![11]));
		assert_ok!(Staking::nominate(RuntimeOrigin::signed(61), vec![11, 21]));

		// nominating with targets above the nomination quota returns error.
		assert_noop!(
			Staking::nominate(RuntimeOrigin::signed(61), vec![11, 21, 31]),
			Error::<Test>::TooManyTargets
		);
	});
}

#[test]
#[should_panic]
#[cfg(debug_assertions)]
fn change_of_absolute_max_nominations() {
	use frame_election_provider_support::ElectionDataProvider;
	ExtBuilder::default()
		.add_staker(61, 10, StakerStatus::Nominator(vec![1]))
		.add_staker(71, 10, StakerStatus::Nominator(vec![1, 2, 3]))
		.balance_factor(10)
		.build_and_execute(|| {
			// pre-condition
			assert_eq!(AbsoluteMaxNominations::get(), 16);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (71, 3), (61, 1)]
			);

			// default bounds are unbounded.
			let bounds = DataProviderBounds::default();

			// 3 validators and 3 nominators
			assert_eq!(Staking::electing_voters(bounds, 0).unwrap().len(), 3 + 3);

			// abrupt change from 16 to 4, everyone should be fine.
			AbsoluteMaxNominations::set(4);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (71, 3), (61, 1)]
			);
			assert_eq!(Staking::electing_voters(bounds, 0).unwrap().len(), 3 + 3);

			// No one can be chilled on account of non-decodable keys.
			for k in Nominators::<Test>::iter_keys() {
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1), k),
					Error::<Test>::CannotChillOther
				);
			}

			// abrupt change from 4 to 3, everyone should be fine.
			AbsoluteMaxNominations::set(3);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (71, 3), (61, 1)]
			);
			assert_eq!(Staking::electing_voters(bounds, 0).unwrap().len(), 3 + 3);

			// As before, no one can be chilled on account of non-decodable keys.
			for k in Nominators::<Test>::iter_keys() {
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1), k),
					Error::<Test>::CannotChillOther
				);
			}

			// abrupt change from 3 to 2, this should cause some nominators to be non-decodable,
			// and thus non-existent unless they update.
			AbsoluteMaxNominations::set(2);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(101, 2), (61, 1)]
			);

			// 101 and 61 still cannot be chilled by someone else.
			for k in [101, 61].iter() {
				assert_noop!(
					Staking::chill_other(RuntimeOrigin::signed(1), *k),
					Error::<Test>::CannotChillOther
				);
			}

			// 71 is still in storage..
			assert!(Nominators::<Test>::contains_key(71));
			// but its value cannot be decoded and default is returned.
			assert!(Nominators::<Test>::get(71).is_none());

			assert_eq!(Staking::electing_voters(bounds, 0).unwrap().len(), 3 + 2);
			assert!(Nominators::<Test>::contains_key(101));

			// abrupt change from 2 to 1, this should cause some nominators to be non-decodable,
			// and thus non-existent unless they update.
			AbsoluteMaxNominations::set(1);

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(61, 1)]
			);

			// 61 *still* cannot be chilled by someone else.
			assert_noop!(
				Staking::chill_other(RuntimeOrigin::signed(1), 61),
				Error::<Test>::CannotChillOther
			);

			assert!(Nominators::<Test>::contains_key(71));
			assert!(Nominators::<Test>::contains_key(61));
			assert!(Nominators::<Test>::get(71).is_none());
			assert!(Nominators::<Test>::get(61).is_some());
			assert_eq!(Staking::electing_voters(bounds, 0).unwrap().len(), 3 + 1);

			// now one of them can revive themselves by re-nominating to a proper value.
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(71), vec![1]));
			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(71, 1), (61, 1)]
			);

			// or they can be chilled by any account.
			assert!(Nominators::<Test>::contains_key(101));
			assert!(Nominators::<Test>::get(101).is_none());
			assert_ok!(Staking::chill_other(RuntimeOrigin::signed(71), 101));
			assert_eq!(*staking_events().last().unwrap(), Event::Chilled { stash: 101 });
			assert!(!Nominators::<Test>::contains_key(101));
			assert!(Nominators::<Test>::get(101).is_none());
		})
}

#[test]
fn nomination_quota_max_changes_decoding() {
	use frame_election_provider_support::ElectionDataProvider;
	ExtBuilder::default()
		.nominate(false)
		.set_status(41, StakerStatus::Validator)
		.add_staker(60, 10, StakerStatus::Nominator(vec![11]))
		.add_staker(70, 10, StakerStatus::Nominator(vec![11, 21, 31]))
		.add_staker(30, 10, StakerStatus::Nominator(vec![11, 21, 31, 41]))
		.add_staker(50, 10, StakerStatus::Nominator(vec![11, 21, 31, 41]))
		.balance_factor(11)
		.build_and_execute(|| {
			// pre-condition.
			assert_eq!(MaxNominationsOf::<Test>::get(), 16);

			let unbounded_election = DataProviderBounds::default();

			assert_eq!(
				Nominators::<Test>::iter()
					.map(|(k, n)| (k, n.targets.len()))
					.collect::<Vec<_>>(),
				vec![(70, 3), (50, 4), (30, 4), (60, 1)]
			);

			// 4 validators and 4 nominators
			assert_eq!(Staking::electing_voters(unbounded_election, 0).unwrap().len(), 4 + 4);
		});
}

#[test]
fn api_nominations_quota_works() {
	ExtBuilder::default().build_and_execute(|| {
		assert_eq!(Staking::api_nominations_quota(10), MaxNominationsOf::<Test>::get());
		assert_eq!(Staking::api_nominations_quota(333), MaxNominationsOf::<Test>::get());
		assert_eq!(Staking::api_nominations_quota(222), 2);
		assert_eq!(Staking::api_nominations_quota(111), 1);
	})
}

#[test]
fn lazy_quota_npos_voters_works_above_quota() {
	ExtBuilder::default()
		.nominate(false)
		// need to make 22, 23, 24 and 25 validators
		.add_staker(22, 1000, StakerStatus::Validator)
		.add_staker(23, 1000, StakerStatus::Validator)
		.add_staker(24, 1000, StakerStatus::Validator)
		.add_staker(25, 1000, StakerStatus::Validator)
		.add_staker(
			61,
			300, // 300 bond has 16 nomination quota.
			StakerStatus::<AccountId>::Nominator(vec![21, 22, 23, 24, 25]),
		)
		.build_and_execute(|| {
			// unbond 78 from stash 60 so that it's bonded balance is 222, which has a lower
			// nomination quota than at nomination time (max 2 targets).
			assert_ok!(Staking::unbond(RuntimeOrigin::signed(61), 78));
			assert_eq!(Staking::api_nominations_quota(300 - 78), 2);

			// even through 61 has nomination quota of 2 at the time of the election, all the
			// nominations (5) will be used.
			assert_eq!(
				Staking::electing_voters(DataProviderBounds::default(), 0)
					.unwrap()
					.iter()
					.map(|(stash, _, targets)| (*stash, targets.len()))
					.collect::<Vec<_>>(),
				vec![(11, 1), (21, 1), (31, 1), (22, 1), (23, 1), (24, 1), (25, 1), (61, 5)],
			);
		});
}

#[test]
fn nominations_quota_limits_size_work() {
	ExtBuilder::default()
		.nominate(false)
		.set_status(41, StakerStatus::Validator)
		.add_staker(71, 333, StakerStatus::<AccountId>::Nominator(vec![11, 21, 31, 41]))
		.build_and_execute(|| {
			// nominations of 71 won't be added due to voter size limit exceeded.
			let bounds = ElectionBoundsBuilder::default().voters_size(101.into()).build();
			assert_eq!(
				Staking::electing_voters(bounds.voters, 0)
					.unwrap()
					.iter()
					.map(|(stash, _, targets)| (*stash, targets.len()))
					.collect::<Vec<_>>(),
				vec![(41, 1), (11, 1), (21, 1), (31, 1)],
			);

			assert_eq!(
				*staking_events().last().unwrap(),
				Event::SnapshotVotersSizeExceeded { size: 100 }
			);

			// however, if the election voter size bounds were larger, the snapshot would
			// include the electing voters of 70.
			let bounds = ElectionBoundsBuilder::default().voters_size(1_000.into()).build();
			assert_eq!(
				Staking::electing_voters(bounds.voters, 0)
					.unwrap()
					.iter()
					.map(|(stash, _, targets)| (*stash, targets.len()))
					.collect::<Vec<_>>(),
				vec![(41, 1), (11, 1), (21, 1), (31, 1), (71, 4)],
			);
		});
}

mod sorted_list_provider {
	use super::*;
	use frame_election_provider_support::SortedListProvider;

	#[test]
	fn re_nominate_does_not_change_counters_or_list() {
		ExtBuilder::default().nominate(true).build_and_execute(|| {
			// given
			let pre_insert_voter_count =
				(Nominators::<Test>::count() + Validators::<Test>::count()) as u32;
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);

			assert_eq!(
				<Test as Config>::VoterList::iter().collect::<Vec<_>>(),
				vec![11, 21, 31, 101]
			);

			// when account 101 renominates
			assert_ok!(Staking::nominate(RuntimeOrigin::signed(101), vec![31]));

			// then counts don't change
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);
			// and the list is the same
			assert_eq!(
				<Test as Config>::VoterList::iter().collect::<Vec<_>>(),
				vec![11, 21, 31, 101]
			);
		});
	}

	#[test]
	fn re_validate_does_not_change_counters_or_list() {
		ExtBuilder::default().nominate(false).build_and_execute(|| {
			// given
			let pre_insert_voter_count =
				(Nominators::<Test>::count() + Validators::<Test>::count()) as u32;
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);

			assert_eq!(<Test as Config>::VoterList::iter().collect::<Vec<_>>(), vec![11, 21, 31]);

			// when account 11 re-validates
			assert_ok!(Staking::validate(RuntimeOrigin::signed(11), Default::default()));

			// then counts don't change
			assert_eq!(<Test as Config>::VoterList::count(), pre_insert_voter_count);
			// and the list is the same
			assert_eq!(<Test as Config>::VoterList::iter().collect::<Vec<_>>(), vec![11, 21, 31]);
		});
	}
}

mod paged_snapshot {
	use super::*;

	#[test]
	fn target_snapshot_works() {
		ExtBuilder::default()
			.nominate(true)
			.set_status(41, StakerStatus::Validator)
			.set_status(51, StakerStatus::Validator)
			.set_status(101, StakerStatus::Idle)
			.build_and_execute(|| {
				// all registered validators.
				let all_targets = vec![51, 31, 41, 21, 11];
				assert_eq_uvec!(
					<Test as Config>::TargetList::iter().collect::<Vec<_>>(),
					all_targets,
				);

				// 3 targets per page.
				let bounds =
					ElectionBoundsBuilder::default().targets_count(3.into()).build().targets;

				let targets =
					<Staking as ElectionDataProvider>::electable_targets(bounds, 0).unwrap();
				assert_eq_uvec!(targets, all_targets.iter().take(3).cloned().collect::<Vec<_>>());

				// emulates a no bounds target snapshot request.
				let bounds =
					ElectionBoundsBuilder::default().targets_count(u32::MAX.into()).build().targets;

				let single_page_targets =
					<Staking as ElectionDataProvider>::electable_targets(bounds, 0).unwrap();

				// complete set of paged targets is the same as single page, no bounds set of
				// targets.
				assert_eq_uvec!(all_targets, single_page_targets);
			})
	}

	#[test]
	fn target_snapshot_multi_page_redundant() {
		ExtBuilder::default().build_and_execute(|| {
			let all_targets = vec![31, 21, 11];
			assert_eq_uvec!(<Test as Config>::TargetList::iter().collect::<Vec<_>>(), all_targets,);

			// no bounds.
			let bounds =
				ElectionBoundsBuilder::default().targets_count(u32::MAX.into()).build().targets;

			// target snapshot supports only single-page, thus it is redundant what's the page index
			// requested.
			let snapshot = Staking::electable_targets(bounds, 0).unwrap();
			assert!(
				snapshot == all_targets &&
					snapshot == Staking::electable_targets(bounds, 1).unwrap() &&
					snapshot == Staking::electable_targets(bounds, 2).unwrap() &&
					snapshot == Staking::electable_targets(bounds, u32::MAX).unwrap(),
			);
		})
	}

	#[test]
	fn voter_snapshot_works() {
		ExtBuilder::default()
			.nominate(true)
			.set_status(51, StakerStatus::Validator)
			.set_status(41, StakerStatus::Nominator(vec![51]))
			.set_status(101, StakerStatus::Validator)
			.build_and_execute(|| {
				let bounds = ElectionBoundsBuilder::default().voters_count(3.into()).build().voters;
				assert_eq!(
					<Test as Config>::VoterList::iter()
						.collect::<Vec<_>>()
						.into_iter()
						.map(|v| (v, <Test as Config>::VoterList::get_score(&v).unwrap()))
						.collect::<Vec<_>>(),
					vec![(51, 5000), (41, 4000), (11, 1000), (21, 1000), (31, 500), (101, 500)],
				);

				let mut all_voters = vec![];

				let voters_page_3 = <Staking as ElectionDataProvider>::electing_voters(bounds, 3)
					.unwrap()
					.into_iter()
					.map(|(a, _, _)| a)
					.collect::<Vec<_>>();
				all_voters.extend(voters_page_3.clone());

				assert_eq!(voters_page_3, vec![51, 41, 11]);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Ongoing(11));

				let voters_page_2 = <Staking as ElectionDataProvider>::electing_voters(bounds, 2)
					.unwrap()
					.into_iter()
					.map(|(a, _, _)| a)
					.collect::<Vec<_>>();
				all_voters.extend(voters_page_2.clone());

				assert_eq!(voters_page_2, vec![21, 31, 101]);

				// all voters in the list have been consumed.
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Consumed);

				// thus page 1 and 0 are empty.
				assert!(<Staking as ElectionDataProvider>::electing_voters(bounds, 1)
					.unwrap()
					.is_empty());
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Consumed);

				assert!(<Staking as ElectionDataProvider>::electing_voters(bounds, 0)
					.unwrap()
					.is_empty());

				// last page has been requested, reset the snapshot status to waiting.
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Waiting);

				// now request 1 page with bounds where all registered voters fit. u32::MAX
				// emulates a no bounds request.
				let bounds =
					ElectionBoundsBuilder::default().voters_count(u32::MAX.into()).build().targets;

				let single_page_voters =
					<Staking as ElectionDataProvider>::electing_voters(bounds, 0)
						.unwrap()
						.into_iter()
						.map(|(a, _, _)| a)
						.collect::<Vec<_>>();

				// complete set of paged voters is the same as single page, no bounds set of
				// voters.
				assert_eq!(all_voters, single_page_voters);
			})
	}

	#[test]
	fn voter_list_locked_during_multi_page_snapshot() {
		ExtBuilder::default()
			.nominate(true)
			.set_status(51, StakerStatus::Validator)
			.set_status(41, StakerStatus::Nominator(vec![51]))
			.set_status(101, StakerStatus::Validator)
			.build_and_execute(|| {
				let bounds = ElectionBoundsBuilder::default().voters_count(2.into()).build().voters;
				assert_eq!(
					<Test as Config>::VoterList::iter()
						.collect::<Vec<_>>()
						.into_iter()
						.map(|v| (v, <Test as Config>::VoterList::get_score(&v).unwrap()))
						.collect::<Vec<_>>(),
					vec![(51, 5000), (41, 4000), (11, 1000), (21, 1000), (31, 500), (101, 500)],
				);

				// initially not locked
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), None);

				let voters_page_3 = <Staking as ElectionDataProvider>::electing_voters(bounds, 3)
					.unwrap()
					.into_iter()
					.map(|(a, _, _)| a)
					.collect::<Vec<_>>();

				assert_eq!(voters_page_3, vec![51, 41]);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Ongoing(41));
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), Some(()));

				hypothetically!({});

				let voters_page_2 = <Staking as ElectionDataProvider>::electing_voters(bounds, 2)
					.unwrap()
					.into_iter()
					.map(|(a, _, _)| a)
					.collect::<Vec<_>>();

				// still locked
				assert_eq!(voters_page_2, vec![11, 21]);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Ongoing(21));
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), Some(()));

				let voters_page_1 = <Staking as ElectionDataProvider>::electing_voters(bounds, 1)
					.unwrap()
					.into_iter()
					.map(|(a, _, _)| a)
					.collect::<Vec<_>>();

				// consumed, and we already unlock
				assert_eq_uvec!(voters_page_1, vec![31, 101]);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Consumed);
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), None);

				// calling page zero will unlock us.
				assert!(<Staking as ElectionDataProvider>::electing_voters(bounds, 0)
					.unwrap()
					.is_empty());

				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Waiting);
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), None);
			})
	}

	#[test]
	fn voter_list_not_updated_when_locked() {
		ExtBuilder::default()
			.nominate(true)
			.set_status(51, StakerStatus::Validator)
			.set_status(41, StakerStatus::Nominator(vec![51]))
			.set_status(101, StakerStatus::Validator)
			.build_and_execute(|| {
				let bounds = ElectionBoundsBuilder::default().voters_count(2.into()).build().voters;
				assert_eq!(
					<Test as Config>::VoterList::iter()
						.collect::<Vec<_>>()
						.into_iter()
						.map(|v| (v, <Test as Config>::VoterList::get_score(&v).unwrap()))
						.collect::<Vec<_>>(),
					vec![(51, 5000), (41, 4000), (11, 1000), (21, 1000), (31, 500), (101, 500)],
				);

				// initial bag of 51
				assert_eq!(
					pallet_bags_list::ListNodes::<T, VoterBagsListInstance>::get(51)
						.unwrap()
						.bag_upper,
					10_000
				);

				// original bag of 11
				assert_eq!(
					pallet_bags_list::ListNodes::<T, VoterBagsListInstance>::get(11)
						.unwrap()
						.bag_upper,
					1000
				);

				// initially not locked
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), None);

				let voters_page_3 = <Staking as ElectionDataProvider>::electing_voters(bounds, 3)
					.unwrap()
					.into_iter()
					.map(|(a, _, _)| a)
					.collect::<Vec<_>>();

				assert_eq!(voters_page_3, vec![51, 41]);
				assert_eq!(VoterSnapshotStatus::<Test>::get(), SnapshotStatus::Ongoing(41));
				assert_eq!(pallet_bags_list::Lock::<T, VoterBagsListInstance>::get(), Some(()));

				// 51 who is already part of the list might want to unbond. They are already in the
				// snapshot, and their position is not updated
				hypothetically!({
					assert_ok!(Staking::unbond(RuntimeOrigin::signed(51), 500));
					// they are still in the original bag
					assert_eq!(
						pallet_bags_list::ListNodes::<T, VoterBagsListInstance>::get(51)
							.unwrap()
							.bag_upper,
						10_000
					);
				});

				// 11 who is not part of the snapshot yet might want to bond a lot extra, this is
				// not reflected in this election.
				hypothetically!({
					crate::asset::set_stakeable_balance::<T>(&11, 10000);
					assert_ok!(Staking::bond_extra(RuntimeOrigin::signed(11), 5000));
					// they are still in the original bag
					assert_eq!(
						pallet_bags_list::ListNodes::<T, VoterBagsListInstance>::get(11)
							.unwrap()
							.bag_upper,
						1000
					);
				});
			})
	}
}

mod score_provider {
	use frame_election_provider_support::ScoreProvider;

	use super::*;

	#[test]
	fn no_score_for_chilled_stakers() {
		ExtBuilder::default().build_and_execute(|| {
			// given 41 being a chilled staker
			assert!(
				Ledger::<Test>::get(41).is_some() &&
					!Validators::<Test>::contains_key(41) &&
					!Nominators::<Test>::contains_key(41)
			);

			// then they will not have a score when bags-list wants to update it.
			assert!(<Staking as ScoreProvider<_>>::score(&41).is_none());
		});
	}

	#[test]
	fn no_score_for_non_stakers() {
		ExtBuilder::default().build_and_execute(|| {
			// given 777 being neither a nominator nor a validator in this pallet.
			assert!(
				!Ledger::<Test>::get(777).is_some() &&
					!Validators::<Test>::contains_key(777) &&
					!Nominators::<Test>::contains_key(777)
			);

			// then it will not have a score when bags-list wants to update it.
			assert!(<Staking as ScoreProvider<_>>::score(&777).is_none());
		});
	}

	#[test]
	fn score_for_validators_nominators() {
		ExtBuilder::default().nominate(true).build_and_execute(|| {
			// Given 101 being a nominator
			assert!(
				Ledger::<Test>::get(101).unwrap().active == 500 &&
					!Validators::<Test>::contains_key(101) &&
					Nominators::<Test>::contains_key(101)
			);

			// then it will have a score.
			assert_eq!(<Staking as ScoreProvider<_>>::score(&101), Some(500));

			// given 11 being a validator
			assert!(
				Ledger::<Test>::get(11).unwrap().active == 1000 &&
					Validators::<Test>::contains_key(11) &&
					!Nominators::<Test>::contains_key(11)
			);

			// then it will have a score.
			assert_eq!(<Staking as ScoreProvider<_>>::score(&11), Some(1000));
		});
	}
}

#[test]
fn from_most_staked_to_least_staked() {
	ExtBuilder::default()
		.nominate(true)
		.set_status(51, StakerStatus::Validator)
		.set_status(41, StakerStatus::Nominator(vec![51]))
		.set_status(101, StakerStatus::Validator)
		.set_stake(41, 11000)
		.set_stake(51, 2500)
		.set_stake(101, 35)
		.build_and_execute(|| {
			assert_eq!(THRESHOLDS.to_vec(), [10, 20, 30, 40, 50, 60, 1_000, 2_000, 10_000]);

			assert_eq!(
				<Test as Config>::VoterList::iter()
					.collect::<Vec<_>>()
					.into_iter()
					.map(|v| (v, <Test as Config>::VoterList::get_score(&v).unwrap()))
					.collect::<Vec<_>>(),
				vec![(41, 11000), (51, 2500), (11, 1000), (21, 1000), (31, 500), (101, 35)],
			);
		});
}
