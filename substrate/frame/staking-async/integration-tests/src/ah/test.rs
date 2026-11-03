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

use crate::{ah::mock::*, rc, shared};
use frame::prelude::Perbill;
use frame_election_provider_support::Weight;
use frame_support::{
	assert_ok, hypothetically,
	traits::fungible::{hold::Inspect as HoldInspect, Inspect, Mutate, Unbalanced},
};
use pallet_election_provider_multi_block::{
	signed::Event as SignedEvent, unsigned::miner::OffchainWorkerMiner,
	verifier::Event as VerifierEvent, CurrentPhase, ElectionScore, Event as ElectionEvent, Phase,
};
use pallet_staking_async::{
	self as staking_async, session_rotation::Rotator, ActiveEra, ActiveEraInfo, CurrentEra,
	DisableMintingGuard, ErasValidatorIncentiveBudget, ErasValidatorReward, Event as StakingEvent,
	PotAccountProvider, RewardKind, RewardPot, SequentialTest,
};
use pallet_staking_async_rc_client::{
	self as rc_client, OutgoingValidatorSet, UnexpectedKind, ValidatorSetReport,
};

// Tests that are specific to Asset Hub.
#[test]
fn on_receive_session_report() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// GIVEN genesis state of ah
		assert_eq!(System::block_number(), 1);
		assert_eq!(CurrentEra::<T>::get(), Some(0));
		assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 0);
		assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));

		// WHEN session ends on RC and session report is received by AH.
		let session_report = rc_client::SessionReport {
			end_index: 0,
			validator_points: (1..9).into_iter().map(|v| (v as AccountId, v * 10)).collect(),
			activation_timestamp: None,
			leftover: false,
		};

		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			session_report.clone(),
		));

		// THEN end 0, start 1, plan 2
		let era_points = staking_async::ErasRewardPoints::<T>::get(&0);
		assert_eq!(era_points.total, 360);
		assert_eq!(era_points.individual.get(&1), Some(&10));
		assert_eq!(era_points.individual.get(&4), Some(&40));
		assert_eq!(era_points.individual.get(&7), Some(&70));
		assert_eq!(era_points.individual.get(&8), Some(&80));
		assert_eq!(era_points.individual.get(&9), None);

		// assert no era changed yet.
		assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));

		// election starts in the 1st session.
		assert_eq!(CurrentEra::<T>::get(), Some(1));
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::SessionRotated {
				starting_session: 1,
				active_era: 0,
				planned_era: 1
			}]
		);

		assert_eq!(
			election_events_since_last_call(),
			// Snapshot phase has started which will run for 3 blocks
			vec![ElectionEvent::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) }]
		);

		// roll 3 blocks for signed phase, and one for the transition.
		roll_many(3 + 1);
		assert_eq!(
			election_events_since_last_call(),
			// Signed phase has started which will run for 3 blocks.
			vec![ElectionEvent::PhaseTransitioned {
				from: Phase::Snapshot(0),
				to: Phase::Signed(3)
			}]
		);

		// roll some blocks until election result is exported.
		roll_many(15);
		assert_eq!(
			election_events_since_last_call(),
			vec![
				ElectionEvent::PhaseTransitioned {
					from: Phase::Signed(0),
					to: Phase::SignedValidation(6)
				},
				ElectionEvent::PhaseTransitioned {
					from: Phase::SignedValidation(0),
					to: Phase::Unsigned(3)
				},
				ElectionEvent::PhaseTransitioned { from: Phase::Unsigned(0), to: Phase::Done },
			]
		);

		// no staking event while election ongoing.
		assert_eq!(staking_events_since_last_call(), vec![]);
		// no xcm message sent yet.
		assert_eq!(LocalQueue::get().unwrap(), vec![]);

		// normal conditions, validator set can be sent.
		// next 3 block exports the election result to staking.
		roll_many(3);

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::PagedElectionProceeded { page: 2, result: Ok(4) },
				StakingEvent::PagedElectionProceeded { page: 1, result: Ok(0) },
				StakingEvent::PagedElectionProceeded { page: 0, result: Ok(0) }
			]
		);

		assert_eq!(
			election_events_since_last_call(),
			vec![
				ElectionEvent::PhaseTransitioned { from: Phase::Done, to: Phase::Export(1) },
				ElectionEvent::PhaseTransitioned { from: Phase::Export(0), to: Phase::Off }
			]
		);

		// outgoing set is queued.
		assert!(OutgoingValidatorSet::<T>::get().is_some());

		// roll three more sessions...
		for _ in 0..3 {
			end_session_with(false, AssertSessionType::IdleNoExport);
		}

		// no election events emitted anymore
		assert_eq!(election_events_since_last_call(), vec![]);

		// At the end of next session we should be ready to export the outgoing set.
		// Note: we don't roll any blocks to show outgoing set gets exported 1 block after the
		// session-ending block
		end_session_with(false, AssertSessionType::IdleOnlyExport);

		// New validator set xcm message is sent to RC.
		assert_eq!(
			LocalQueue::get().unwrap(),
			vec![(
				// this is the block number at which the message was sent.
				System::block_number(),
				OutgoingMessages::ValidatorSet(ValidatorSetReport {
					new_validator_set: vec![3, 5, 6, 8],
					id: 1,
					prune_up_to: None,
					leftover: false
				})
			)]
		);

		// Validator is queued in session pallet for 1 session. We wait to activate.
		end_session_with(false, AssertSessionType::IdleNoExport);

		// now we activate era
		end_session_with(true, AssertSessionType::ElectionWithBufferedExport);
	})
}

#[test]
fn validator_set_send_fail_retries() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// GIVEN genesis state of ah
		assert_eq!(System::block_number(), 1);
		assert_eq!(CurrentEra::<T>::get(), Some(0));
		assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 0);
		assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));
		// flush old events.
		let _ = staking_events_since_last_call();

		// first session comes in.
		let session_report = rc_client::SessionReport {
			end_index: 0,
			validator_points: (1..9).into_iter().map(|v| (v as AccountId, v * 10)).collect(),
			activation_timestamp: None,
			leftover: false,
		};

		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			session_report.clone(),
		));

		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::SessionRotated {
				starting_session: 1,
				active_era: 0,
				// planned era 1 indicates election start signal is sent.
				planned_era: 1
			}]
		);

		assert_eq!(
			election_events_since_last_call(),
			// Snapshot phase has started which will run for 3 blocks
			vec![ElectionEvent::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) }]
		);

		// roll 3 blocks for signed phase, and one for the transition.
		roll_many(3 + 1);
		assert_eq!(
			election_events_since_last_call(),
			// Signed phase has started which will run for 3 blocks.
			vec![ElectionEvent::PhaseTransitioned {
				from: Phase::Snapshot(0),
				to: Phase::Signed(3)
			}]
		);

		// roll some blocks until election result is exported.
		roll_many(15);
		assert_eq!(
			election_events_since_last_call(),
			vec![
				ElectionEvent::PhaseTransitioned {
					from: Phase::Signed(0),
					to: Phase::SignedValidation(6)
				},
				ElectionEvent::PhaseTransitioned {
					from: Phase::SignedValidation(0),
					to: Phase::Unsigned(3)
				},
				ElectionEvent::PhaseTransitioned { from: Phase::Unsigned(0), to: Phase::Done },
			]
		);

		// no staking event while election ongoing.
		assert_eq!(staking_events_since_last_call(), vec![]);

		roll_many(3);
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::PagedElectionProceeded { page: 2, result: Ok(4) },
				StakingEvent::PagedElectionProceeded { page: 1, result: Ok(0) },
				StakingEvent::PagedElectionProceeded { page: 0, result: Ok(0) }
			]
		);

		assert_eq!(
			election_events_since_last_call(),
			vec![
				ElectionEvent::PhaseTransitioned { from: Phase::Done, to: Phase::Export(1) },
				ElectionEvent::PhaseTransitioned { from: Phase::Export(0), to: Phase::Off }
			]
		);

		// assert validator set queued in rc client pallet
		assert!(OutgoingValidatorSet::<T>::exists());

		// roll until session 3
		for _ in 0..3 {
			end_session_with(false, AssertSessionType::IdleNoExport);
		}

		// no xcm message sent yet.
		assert_eq!(LocalQueue::get().unwrap(), vec![]);

		// Next session we will try to export the validator set, but we test the bad condition.
		// -- assume the next validator set cannot be sent.
		NextRelayDeliveryFails::set(true);
		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			rc_client::SessionReport {
				end_index: 4,
				validator_points: vec![(1, 10)],
				activation_timestamp: None,
				leftover: false,
			}
		));

		// nothing is queued
		assert!(LocalQueue::get().unwrap().is_empty());

		// clear rc client events
		let _ = rc_client_events_since_last_call();

		// next block should try to export validator set but fail
		roll_next();

		// rc-client has an event
		assert_eq!(
			rc_client_events_since_last_call(),
			vec![rc_client::Event::Unexpected(UnexpectedKind::ValidatorSetSendFailed)]
		);

		// the buffer is set
		assert!(matches!(OutgoingValidatorSet::<T>::get(), Some((_, 2))));

		// next block it is retried and sent fine
		roll_next();
		assert_eq!(
			LocalQueue::get().unwrap(),
			vec![(
				// this is the block number at which the message was sent.
				System::block_number(),
				OutgoingMessages::ValidatorSet(ValidatorSetReport {
					new_validator_set: vec![3, 5, 6, 8],
					id: 1,
					prune_up_to: None,
					leftover: false
				})
			)]
		);

		// buffer is clear
		assert!(OutgoingValidatorSet::<T>::get().is_none());
	});
}

#[test]
fn roll_many_eras() {
	// todo:
	// - Ensure rewards can be claimed at correct era.
	// - assert outgoing messages, including id and prune_up_to.
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// ERA 0 -> 1
		let mut active_era = ActiveEra::<T>::get().unwrap().index;
		let mut current_era = CurrentEra::<T>::get().unwrap();
		assert_eq!(active_era, 0);
		assert_eq!(current_era, 0);

		// -- first session will start the election
		end_session_with(false, AssertSessionType::ElectionWithBufferedExport);
		// active era not incremented
		assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
		// current era incremented indicating election started.
		current_era += 1;
		assert_eq!(CurrentEra::<T>::get().unwrap(), current_era);

		// -- next 2 sessions are idle
		for _ in 0..2 {
			end_session_with(false, AssertSessionType::IdleNoExport);
			assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
			assert_eq!(CurrentEra::<T>::get().unwrap(), current_era);
		}

		// next session exports the set
		end_session_with(false, AssertSessionType::IdleOnlyExport);

		// another idle session
		end_session_with(false, AssertSessionType::IdleNoExport);

		// end of 6th we get activation stamp
		// (this also combines as election trigger session for the next era)
		end_session_with(true, AssertSessionType::ElectionWithBufferedExport);
		active_era += 1;
		assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
		// election for next already starts
		current_era += 1;
		assert_eq!(CurrentEra::<T>::get().unwrap(), current_era);

		// run next 49 eras
		for active_era in 1..50 {
			// election has already started
			assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
			assert_eq!(CurrentEra::<T>::get().unwrap(), active_era + 1);

			// --- Before end of session 4: validator set is ready
			for _ in 0..3 {
				end_session_with(false, AssertSessionType::IdleNoExport);
				assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
				assert_eq!(CurrentEra::<T>::get().unwrap(), active_era + 1);
			}

			assert!(OutgoingValidatorSet::<T>::exists());
			// ensure validator set not sent yet to RC.
			// queue size same as in last iteration.
			assert_eq!(LocalQueue::get().unwrap().len() as u32, active_era);

			// --- validator set is exported in the 5th session.
			end_session_with(false, AssertSessionType::IdleOnlyExport);
			assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
			assert_eq!(CurrentEra::<T>::get().unwrap(), active_era + 1);
			assert_eq!(LocalQueue::get().unwrap().len() as u32, active_era + 1);

			// --- end session 5 (activation will be next session)
			end_session_with(false, AssertSessionType::IdleNoExport);
			assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era);
			assert_eq!(CurrentEra::<T>::get().unwrap(), active_era + 1);

			// --- 6th the era rotation session
			end_session_with(true, AssertSessionType::ElectionWithBufferedExport);
			assert_eq!(ActiveEra::<T>::get().unwrap().index, active_era + 1);
			// next election starts
			assert_eq!(CurrentEra::<T>::get().unwrap(), active_era + 2);
		}
	});
}

#[test]
fn receives_old_session_report() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// Initial state
		assert_eq!(CurrentEra::<T>::get(), Some(0));
		assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 0);
		assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), None);

		// Receive report for end of 0, start of 1 and plan 2.
		let session_report = rc_client::SessionReport {
			end_index: 0,
			validator_points: vec![(5, 50)],
			activation_timestamp: None,
			leftover: false,
		};

		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			session_report.clone(),
		));

		// then
		assert_eq!(
			rc_client_events_since_last_call(),
			vec![rc_client::Event::SessionReportReceived {
				end_index: 0,
				activation_timestamp: None,
				validator_points_counts: 1,
				leftover: false
			}]
		);
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::SessionRotated {
				starting_session: 1,
				active_era: 0,
				planned_era: 1
			}]
		);

		// reward points are not added
		assert_eq!(staking_async::ErasRewardPoints::<T>::get(&0).total, 50);
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), Some(0));

		// then send it again, this is basically dropped, although it returns `Ok()`
		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			session_report
		));

		// no storage is changed
		assert_eq!(staking_async::ErasRewardPoints::<T>::get(&0).total, 50);
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), Some(0));
	})
}

#[test]
fn receives_session_report_in_future() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// Initial state
		assert_eq!(CurrentEra::<T>::get(), Some(0));
		assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 0);
		assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), None);

		// Receive report for end of 0, start of 1 and plan 2.
		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			rc_client::SessionReport {
				end_index: 0,
				validator_points: vec![(5, 50)],
				activation_timestamp: None,
				leftover: false,
			},
		));

		// THEN:
		// last session index is updated
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), Some(0));

		assert_eq!(
			rc_client_events_since_last_call(),
			vec![rc_client::Event::SessionReportReceived {
				end_index: 0,
				activation_timestamp: None,
				validator_points_counts: 1,
				leftover: false
			}]
		);

		// Election starts now asap
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::SessionRotated {
				starting_session: 1,
				active_era: 0,
				planned_era: 1
			}]
		);

		// reward points are added
		assert_eq!(staking_async::ErasRewardPoints::<T>::get(&0).total, 50);

		// skip end_index 1, send 2
		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			rc_client::SessionReport {
				end_index: 2,
				validator_points: vec![(5, 50)],
				activation_timestamp: None,
				leftover: false,
			},
		));

		// our tracker of last session is updated (and has skipped `1`)
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), Some(2));

		assert_eq!(
			rc_client_events_since_last_call(),
			vec![
				rc_client::Event::Unexpected(UnexpectedKind::SessionSkipped),
				rc_client::Event::SessionReportReceived {
					end_index: 2,
					activation_timestamp: None,
					validator_points_counts: 1,
					leftover: false
				}
			]
		);
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::SessionRotated {
				starting_session: 3,
				active_era: 0,
				planned_era: 1
			}]
		);

		// reward points are accumulated
		assert_eq!(staking_async::ErasRewardPoints::<T>::get(&0).total, 100);
	})
}

#[test]
fn session_report_burst() {
	// note: there is also an e2e `session_report_burst` test
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// Initial state
		assert_eq!(CurrentEra::<T>::get(), Some(0));
		assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 0);
		assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));
		assert_eq!(rc_client::LastSessionReportEndingIndex::<T>::get(), None);

		// then send 20 sessions all at once. This is enough to schedule multiple elections, but we
		// only schedule one.
		for s in 1..=20 {
			assert_ok!(rc_client::Pallet::<T>::relay_session_report(
				RuntimeOrigin::root(),
				rc_client::SessionReport {
					end_index: s,
					validator_points: vec![(5, 50)],
					activation_timestamp: None,
					leftover: false,
				},
			));
			// all are processed fine, in one go
			assert_eq!(
				rc_client_events_since_last_call(),
				vec![rc_client::Event::SessionReportReceived {
					end_index: s,
					activation_timestamp: None,
					validator_points_counts: 1,
					leftover: false
				}]
			);
			// and we have collected reward points.
			assert_eq!(staking_async::ErasRewardPoints::<T>::get(&0).total, 50 * s);
		}

		// crucially, election has started, but we have not done anything else.
		Rotator::<T>::assert_election_ongoing();

		assert!(matches!(
			&staking_events_since_last_call()[..],
			&[
				staking_async::Event::SessionRotated {
					starting_session: 2,
					active_era: 0,
					planned_era: 1
				},
				..,
				staking_async::Event::SessionRotated {
					starting_session: 21,
					active_era: 0,
					planned_era: 1
				}
			]
		));
	})
}

#[test]
fn on_offence_current_era() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		let active_validators = roll_until_next_active(0);
		assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 7);
		assert_eq!(active_validators, vec![3, 5, 6, 8]);

		// flush the events.
		let _ = staking_events_since_last_call();

		assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
			RuntimeOrigin::root(),
			vec![
				(
					7,
					rc_client::Offence {
						offender: 5,
						reporters: vec![],
						slash_fraction: Perbill::from_percent(50),
					}
				),
				(
					7,
					rc_client::Offence {
						offender: 3,
						reporters: vec![],
						slash_fraction: Perbill::from_percent(50),
					}
				)
			]
		));

		assert_eq!(
			staking_events_since_last_call(),
			vec![
				staking_async::Event::OffenceReported {
					offence_era: 1,
					validator: 5,
					fraction: Perbill::from_percent(50)
				},
				staking_async::Event::OffenceReported {
					offence_era: 1,
					validator: 3,
					fraction: Perbill::from_percent(50)
				}
			]
		);

		// 2 blocks to process these offences, and they are deferred.
		roll_next();
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::SlashComputed {
				offence_era: 1,
				slash_era: 3,
				offender: 5,
				page: 0,
			}]
		);
		roll_next();
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::SlashComputed {
				offence_era: 1,
				slash_era: 3,
				offender: 3,
				page: 0,
			}]
		);

		// skip two eras
		assert_eq!(SlashDeferredDuration::get(), 2);
		roll_until_next_active(7);
		roll_until_next_active(13);
		let _ = staking_events_since_last_call();

		// 2 blocks to apply the slashes
		roll_next();
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::Slashed { staker: 3, amount: 50 },]
		);
		roll_next();
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				staking_async::Event::Slashed { staker: 5, amount: 50 },
				staking_async::Event::Slashed { staker: 110, amount: 50 }
			]
		);
	});
}

#[test]
fn on_offence_current_era_instant_apply() {
	ExtBuilder::default()
		.local_queue()
		.slash_defer_duration(0)
		.build()
		.execute_with(|| {
			let active_validators = roll_until_next_active(0);
			assert_eq!(Rotator::<Runtime>::era_start_session_index(1), Some(7));
			assert_eq!(active_validators, vec![3, 5, 6, 8]);

			// flush the events.
			let _ = staking_events_since_last_call();

			let dap_buffer = <pallet_dap::Pallet<Runtime> as sp_staking::budget::BudgetRecipient<
				AccountId,
			>>::pot_account();

			assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
				RuntimeOrigin::root(),
				vec![
					(
						7,
						rc_client::Offence {
							offender: 5,
							reporters: vec![],
							slash_fraction: Perbill::from_percent(50),
						}
					),
					(
						7,
						rc_client::Offence {
							offender: 3,
							reporters: vec![],
							slash_fraction: Perbill::from_percent(50),
						}
					)
				]
			));

			assert_eq!(
				staking_events_since_last_call(),
				vec![
					staking_async::Event::OffenceReported {
						offence_era: 1,
						validator: 5,
						fraction: Perbill::from_percent(50)
					},
					staking_async::Event::OffenceReported {
						offence_era: 1,
						validator: 3,
						fraction: Perbill::from_percent(50)
					}
				]
			);

			// Capture state before slash application (after DAP drips have occurred).
			let initial_dap_balance = Balances::free_balance(&dap_buffer);
			let initial_total_issuance = Balances::total_issuance();

			// 2 blocks to process these offences, and they are applied on the spot.
			roll_next();
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					StakingEvent::SlashComputed {
						offence_era: 1,
						slash_era: 1,
						offender: 5,
						page: 0,
					},
					StakingEvent::Slashed { staker: 5, amount: 50 },
					StakingEvent::Slashed { staker: 110, amount: 50 },
				]
			);
			roll_next();
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					StakingEvent::SlashComputed {
						offence_era: 1,
						slash_era: 1,
						offender: 3,
						page: 0,
					},
					StakingEvent::Slashed { staker: 3, amount: 50 },
				]
			);

			// DAP verification: each block drips 12_000ms of inflation. Budget is 50/50,
			// so buffer gets 6_000 per block. Two slash-processing blocks = 12_000 drip.
			// Plus 150 from slashes (50 + 50 + 50).
			let final_dap_balance = Balances::free_balance(&dap_buffer);
			assert_eq!(final_dap_balance - initial_dap_balance, 12_000 + 150);

			// Slashing redistributes (no burn), DAP mints 2 * 12_000 = 24_000.
			let final_total_issuance = Balances::total_issuance();
			assert_eq!(final_total_issuance, initial_total_issuance + 24_000);
		});
}

#[test]
fn on_offence_non_validator() {
	ExtBuilder::default()
		.slash_defer_duration(0)
		.local_queue()
		.build()
		.execute_with(|| {
			let active_validators = roll_until_next_active(0);
			assert_eq!(Rotator::<Runtime>::era_start_session_index(1), Some(7));
			assert_eq!(active_validators, vec![3, 5, 6, 8]);

			// flush the events.
			let _ = staking_events_since_last_call();

			assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
				RuntimeOrigin::root(),
				vec![(
					7,
					rc_client::Offence {
						// this offender is unknown to the staking pallet.
						offender: 666,
						reporters: vec![],
						slash_fraction: Perbill::from_percent(50),
					}
				)]
			));

			// nada
			assert_eq!(staking_events_since_last_call(), vec![]);
		});
}

#[test]
fn on_offence_previous_era() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		let _ = roll_until_next_active(0);
		let _ = roll_until_next_active(7);
		let active_validators = roll_until_next_active(13);

		assert_eq!(active_validators, vec![3, 5, 6, 8]);
		assert_eq!(Rotator::<Runtime>::active_era(), 3);

		// flush the events.
		let _ = staking_events_since_last_call();

		// GIVEN slash defer duration of 2 eras, active era = 3.
		assert_eq!(SlashDeferredDuration::get(), 2);
		assert_eq!(Rotator::<Runtime>::era_start_session_index(1), Some(7));
		// 1 era is reserved for the application of slashes.
		let oldest_reportable_era =
			Rotator::<Runtime>::active_era() - (SlashDeferredDuration::get() - 1);
		assert_eq!(oldest_reportable_era, 2);

		// WHEN we report an offence older than Era 2 (oldest reportable era).
		assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
			RuntimeOrigin::root(),
			// offence is in era 1
			vec![(
				7,
				rc_client::Offence {
					offender: 3,
					reporters: vec![],
					slash_fraction: Perbill::from_percent(30),
				}
			)]
		));

		// THEN offence is ignored.
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::OffenceTooOld {
				offence_era: 1,
				validator: 3,
				fraction: Perbill::from_percent(30)
			}]
		);

		// WHEN: report an offence for the session belonging to the previous era
		assert_eq!(Rotator::<Runtime>::era_start_session_index(2), Some(13));
		assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
			RuntimeOrigin::root(),
			// offence is in era 2
			vec![(
				13,
				rc_client::Offence {
					offender: 3,
					reporters: vec![],
					slash_fraction: Perbill::from_percent(50),
				}
			)]
		));

		// THEN: offence is reported.
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::OffenceReported {
				offence_era: 2,
				validator: 3,
				fraction: Perbill::from_percent(50)
			}]
		);

		// computed in the next block (will be applied in era 4)
		roll_next();
		assert_eq!(
			staking_events_since_last_call(),
			vec![StakingEvent::SlashComputed {
				offence_era: 2,
				slash_era: 4,
				offender: 3,
				page: 0,
			}]
		);

		// roll to the next era.
		roll_until_next_active(19);
		// ensure we are in era 4.
		assert_eq!(Rotator::<Runtime>::active_era(), 4);
		// clear staking events.
		let _ = staking_events_since_last_call();

		// the next block applies the slashes.
		roll_next();
		assert_eq!(
			staking_events_since_last_call(),
			vec![staking_async::Event::Slashed { staker: 3, amount: 50 }]
		);

		// nothing left
		roll_next();
		assert_eq!(staking_events_since_last_call(), vec![]);
	});
}

#[test]
fn on_offence_previous_era_instant_apply() {
	ExtBuilder::default()
		.slash_defer_duration(0)
		.local_queue()
		.build()
		.execute_with(|| {
			let _ = roll_until_next_active(0);
			let active_validators = roll_until_next_active(7);

			assert_eq!(active_validators, vec![3, 5, 6, 8]);
			assert_eq!(Rotator::<Runtime>::active_era(), 2);

			// flush the events.
			let _ = staking_events_since_last_call();

			// report an offence for the session belonging to the previous era
			assert_eq!(Rotator::<Runtime>::era_start_session_index(1), Some(7));

			assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
				RuntimeOrigin::root(),
				// offence is in era 1
				vec![(
					7,
					rc_client::Offence {
						offender: 3,
						reporters: vec![],
						slash_fraction: Perbill::from_percent(50),
					}
				)]
			));

			// reported
			assert_eq!(
				staking_events_since_last_call(),
				vec![staking_async::Event::OffenceReported {
					offence_era: 1,
					validator: 3,
					fraction: Perbill::from_percent(50)
				}]
			);

			// applied right away
			roll_next();
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					StakingEvent::SlashComputed {
						offence_era: 1,
						slash_era: 1,
						offender: 3,
						page: 0,
					},
					StakingEvent::Slashed { staker: 3, amount: 50 },
				]
			);

			// nothing left
			roll_next();
			assert_eq!(staking_events_since_last_call(), vec![]);
		});
}

#[test]
fn era_lifecycle_test() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// this is the v1 mode.
		let immediate_export_mode = || {
			// election kicks off at the start of (6-2) 4th session.
			PlanningEraOffset::set(2);
			// export validator set as soon as its received.
			ValidatorSetExportSession::set(0);
		};
		// this is v2 mode we are transitioning to.
		let buffered_export_mode = || {
			// election kicks at the start of 1st session of the era.
			PlanningEraOffset::set(6);
			// export validator set at the end of 4th session.
			ValidatorSetExportSession::set(4);
		};

		// lets start with immediate export mode
		immediate_export_mode();

		assert_eq!(ActiveEra::<T>::get().unwrap().index, 0);

		// -- Era transition 0 -> 1
		// there are 3 idle sessions but session 0 is already ended, so we end two more.
		end_session_with(false, AssertSessionType::IdleNoExport);
		end_session_with(false, AssertSessionType::IdleNoExport);
		// this will end session 3, and assert session 4 is election session.
		end_session_with(false, AssertSessionType::ElectionWithImmediateExport);
		end_session_with(false, AssertSessionType::IdleNoExport);
		end_session_with(true, AssertSessionType::IdleNoExport);

		assert_eq!(ActiveEra::<T>::get().unwrap().index, 1);

		// -- Era transition 1 -> 5
		for era in 1..5 {
			// 3 idle sessions
			for _ in 0..3 {
				end_session_with(false, AssertSessionType::IdleNoExport);
			}
			// this will end session 3, and assert session 4 is election session.
			end_session_with(false, AssertSessionType::ElectionWithImmediateExport);
			end_session_with(false, AssertSessionType::IdleNoExport);
			end_session_with(true, AssertSessionType::IdleNoExport);

			assert_eq!(ActiveEra::<T>::get().unwrap().index, era + 1);
		}

		// we are currently at era 5
		assert_eq!(ActiveEra::<T>::get().unwrap().index, 5);
		// lets switch to buffered export mode and earlier election
		buffered_export_mode();
		// Note: Election didn't kick off since we were in immediate export.
		// Next session should kick off the election without exporting
		end_session_with(false, AssertSessionType::ElectionWithBufferedExport);
		// roll 2 idle sessions. Validator set should still be not exported yet.
		end_session_with(false, AssertSessionType::IdleNoExport);
		end_session_with(false, AssertSessionType::IdleNoExport);
		// end of this session (start of next) will export the validator set.
		end_session_with(false, AssertSessionType::IdleOnlyExport);
		// fifth session should be idle with no export
		end_session_with(false, AssertSessionType::IdleNoExport);
		// and finally era activation
		end_session_with(true, AssertSessionType::ElectionWithBufferedExport);

		// roll few more eras

		for era in 6..10 {
			assert_eq!(ActiveEra::<T>::get().unwrap().index, era);
			// next session should kick off the election without exporting
			end_session_with(false, AssertSessionType::IdleNoExport);
			// roll 2 idle sessions. Validator set should still be not exported yet.
			end_session_with(false, AssertSessionType::IdleNoExport);
			end_session_with(false, AssertSessionType::IdleNoExport);
			// end of this session (start of next) will export the validator set.
			end_session_with(false, AssertSessionType::IdleOnlyExport);
			// fifth session should be idle with no export
			end_session_with(false, AssertSessionType::IdleNoExport);
			// and finally era activation
			end_session_with(true, AssertSessionType::ElectionWithBufferedExport);
			assert_eq!(ActiveEra::<T>::get().unwrap().index, era + 1);
		}
	});
}
mod poll_operations {
	use super::*;
	use pallet_election_provider_multi_block::verifier::{Status, Verifier};

	#[test]
	fn full_election_cycle_with_occasional_out_of_weight_completes() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// given initial state of AH
			assert_eq!(System::block_number(), 1);
			assert_eq!(CurrentEra::<T>::get(), Some(0));
			assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 0);
			assert_eq!(ActiveEra::<T>::get(), Some(ActiveEraInfo { index: 0, start: Some(0) }));
			assert!(OutgoingValidatorSet::<T>::get().is_none());

			// receive session 1 which causes election to start
			assert_ok!(rc_client::Pallet::<T>::relay_session_report(
				RuntimeOrigin::root(),
				rc_client::SessionReport {
					end_index: 0,
					validator_points: vec![(1, 10)],
					activation_timestamp: None,
					leftover: false,
				}
			));

			assert_eq!(
				staking_events_since_last_call(),
				vec![StakingEvent::SessionRotated {
					starting_session: 1,
					active_era: 0,
					// planned era 1 indicates election start signal is sent.
					planned_era: 1
				}]
			);

			assert_eq!(
				election_events_since_last_call(),
				// Snapshot phase has started which will run for 3 blocks
				vec![ElectionEvent::PhaseTransitioned { from: Phase::Off, to: Phase::Snapshot(3) }]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Snapshot(3));

			// create 1 snapshot page normally
			roll_next();
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Snapshot(2));

			// next block won't have enough weight
			NextPollWeight::set(Some(crate::ah::weights::SMALL));
			roll_next();
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::UnexpectedPhaseTransitionOutOfWeight {
					from: Phase::Snapshot(2),
					to: Phase::Snapshot(1),
					required: Weight::from_parts(100, 0),
					had: Weight::from_parts(10, 0)
				}]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Snapshot(2));

			// next 2 blocks happen fine
			roll_next();
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Snapshot(1));

			roll_next();
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Snapshot(0));

			// transition to signed
			roll_next();
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::PhaseTransitioned {
					from: Phase::Snapshot(0),
					to: Phase::Signed(3)
				}]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Signed(3));

			// roll 1
			roll_next();
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Signed(2));

			// unlikely: we have zero weight, we won't progress
			NextPollWeight::set(Some(Weight::default()));
			roll_next();
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::UnexpectedPhaseTransitionOutOfWeight {
					from: Phase::Signed(2),
					to: Phase::Signed(1),
					required: Weight::from_parts(10, 0),
					had: Weight::from_parts(0, 0)
				}]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Signed(2));

			// submit a signed solution
			let solution = OffchainWorkerMiner::<T>::mine_solution(3, true).unwrap();
			assert_ok!(MultiBlockSigned::register(RuntimeOrigin::signed(1), solution.score));
			for (index, page) in solution.solution_pages.into_iter().enumerate() {
				assert_ok!(MultiBlockSigned::submit_page(
					RuntimeOrigin::signed(1),
					index as u32,
					Some(Box::new(page))
				));
			}

			// go to signed validation
			roll_until_matches(|| CurrentPhase::<T>::get() == Phase::SignedValidation(6), false);
			assert_eq!(MultiBlockVerifier::status_storage(), Status::Ongoing(2));
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::PhaseTransitioned {
					from: Phase::Signed(0),
					to: Phase::SignedValidation(6)
				}]
			);

			// first block rolls okay
			roll_next();
			assert_eq!(verifier_events_since_last_call(), vec![VerifierEvent::Verified(2, 4)]);
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::SignedValidation(5));
			assert_eq!(MultiBlockVerifier::status_storage(), Status::Ongoing(1));

			// next block has not enough weight left for verification (verification of non-terminal
			// pages requires MEDIUM)
			NextPollWeight::set(Some(crate::ah::weights::SMALL));
			roll_next();
			assert_eq!(verifier_events_since_last_call(), vec![]);
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::UnexpectedPhaseTransitionOutOfWeight {
					from: Phase::SignedValidation(5),
					to: Phase::SignedValidation(4),
					required: Weight::from_parts(1010, 0),
					had: Weight::from_parts(10, 0)
				}]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::SignedValidation(5));
			assert_eq!(MultiBlockVerifier::status_storage(), Status::Ongoing(1));

			// rest go by fine, roll until done
			roll_until_matches(|| CurrentPhase::<T>::get() == Phase::Done, false);
			assert_eq!(
				verifier_events_since_last_call(),
				vec![
					VerifierEvent::Verified(1, 0),
					VerifierEvent::Verified(0, 3),
					VerifierEvent::Queued(
						ElectionScore {
							minimal_stake: 100,
							sum_stake: 800,
							sum_stake_squared: 180000
						},
						None
					)
				]
			);
			assert_eq!(
				election_events_since_last_call(),
				vec![
					ElectionEvent::PhaseTransitioned {
						from: Phase::SignedValidation(0),
						to: Phase::Unsigned(3)
					},
					ElectionEvent::PhaseTransitioned { from: Phase::Unsigned(0), to: Phase::Done },
				]
			);

			// first export page goes by fine
			assert_eq!(pallet_staking_async::NextElectionPage::<T>::get(), None);
			roll_next();
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::PhaseTransitioned { from: Phase::Done, to: Phase::Export(1) }]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Export(1));
			assert_eq!(
				staking_events_since_last_call(),
				vec![StakingEvent::PagedElectionProceeded { page: 2, result: Ok(4) }]
			);
			assert_eq!(pallet_staking_async::NextElectionPage::<T>::get(), Some(1));

			// second page goes by fine
			roll_next();
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Export(0));
			assert_eq!(
				staking_events_since_last_call(),
				vec![StakingEvent::PagedElectionProceeded { page: 1, result: Ok(0) }]
			);
			assert_eq!(pallet_staking_async::NextElectionPage::<T>::get(), Some(0));

			// last (LARGE page) runs out of weight
			NextPollWeight::set(Some(crate::ah::weights::MEDIUM));
			roll_next();
			assert_eq!(election_events_since_last_call(), vec![]);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Export(0));
			assert_eq!(
				staking_events_since_last_call(),
				vec![StakingEvent::Unexpected(
					pallet_staking_async::UnexpectedKind::PagedElectionOutOfWeight {
						page: 0,
						required: Weight::from_parts(1000, 0),
						had: Weight::from_parts(100, 0)
					}
				)]
			);
			assert_eq!(pallet_staking_async::NextElectionPage::<T>::get(), Some(0));

			// next time it goes by fine
			roll_next();
			assert_eq!(
				election_events_since_last_call(),
				vec![ElectionEvent::PhaseTransitioned { from: Phase::Export(0), to: Phase::Off }]
			);
			assert_eq!(CurrentPhase::<T>::get(), Phase::Off);
			assert_eq!(
				staking_events_since_last_call(),
				vec![StakingEvent::PagedElectionProceeded { page: 0, result: Ok(0) }]
			);
			assert_eq!(pallet_staking_async::NextElectionPage::<T>::get(), None);

			// outgoing message is queued
			assert!(OutgoingValidatorSet::<T>::get().is_some());
		})
	}

	#[test]
	fn slashing_processing_while_election() {
		// This is merely a more realistic example of the above. As staking is ready to receive the
		// election result, an ongoing slash will cause too much weight to be consumed
		// on-initialize, causing not enough weight in the on-poll to process. Everything works as
		// expected, but we get a bit slow.
		//
		// The only other meaningful difference is here that we see in action that first on-init
		// runs, and then the leftover weight is given to on-poll. This is done through the mock
		// setup of this test.
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// first, we roll 1 era so have some validators to slash
			let active_validators = roll_until_next_active(0);
			let _ = staking_events_since_last_call();

			// given initial state of AH
			assert_eq!(System::block_number(), 28);
			// active era is 1
			assert_eq!(ActiveEra::<T>::get().unwrap().index, 1);
			// election for era 2 has started
			assert_eq!(CurrentEra::<T>::get(), Some(2));
			assert_eq!(Rotator::<Runtime>::active_era_start_session_index(), 7);
			let active_era = ActiveEra::<T>::get().unwrap();
			assert_eq!(active_era.index, 1);
			assert!(active_era.start.is_some());
			assert!(OutgoingValidatorSet::<T>::get().is_none());

			// roll until signed and submit a solution.
			roll_until_matches(|| MultiBlock::current_phase().is_signed(), false);
			let solution = OffchainWorkerMiner::<T>::mine_solution(3, true).unwrap();
			assert_ok!(MultiBlockSigned::register(RuntimeOrigin::signed(1), solution.score));
			for (index, page) in solution.solution_pages.into_iter().enumerate() {
				assert_ok!(MultiBlockSigned::submit_page(
					RuntimeOrigin::signed(1),
					index as u32,
					Some(Box::new(page))
				));
			}

			// then roll to done, waiting for staking to start processing it. Indeed, something is
			// queued for export now.
			roll_until_matches(|| MultiBlock::current_phase().is_done(), false);
			assert!(MultiBlockVerifier::queued_score().is_some());

			assert_ok!(rc_client::Pallet::<Runtime>::relay_new_offence_paged(
				RuntimeOrigin::root(),
				vec![(
					// index of the last received session report
					8,
					rc_client::Offence {
						offender: active_validators[0],
						reporters: vec![],
						slash_fraction: Perbill::from_percent(10),
					}
				)]
			));

			assert_eq!(
				staking_events_since_last_call(),
				vec![StakingEvent::OffenceReported {
					offence_era: 1,
					validator: active_validators[0],
					fraction: Perbill::from_percent(10)
				}]
			);

			assert!(pallet_staking_async::NextElectionPage::<T>::get().is_none());

			// now as we roll-next, because weight of `process_offence_queue` is max block...
			roll_next();
			// staking has not moved forward in terms of fetching election pages
			assert!(pallet_staking_async::NextElectionPage::<T>::get().is_none());
			// same with our EPMB
			assert!(MultiBlock::current_phase().is_done());
			// and for tracking we have
			assert_eq!(
				staking_events_since_last_call(),
				vec![
					// slash processing happened..
					StakingEvent::SlashComputed {
						offence_era: 1,
						slash_era: 3,
						offender: active_validators[0],
						page: 0
					},
					// but not this.
					StakingEvent::Unexpected(
						pallet_staking_async::UnexpectedKind::PagedElectionOutOfWeight {
							page: 2,
							required: Weight::from_parts(100, 0),
							had: Weight::from_parts(0, 0)
						}
					)
				]
			);
		});
	}
}

#[test]
fn signed_slash_routes_to_dap_not_burned() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			rc_client::SessionReport {
				end_index: 0,
				validator_points: vec![(1, 10)],
				activation_timestamp: None,
				leftover: false,
			}
		));

		roll_until_matches(|| MultiBlock::current_phase().is_signed(), false);

		// Register but never submit any pages: this claim is infeasible and will be rejected
		// (and slashed) once the validation phase evaluates it.
		let invalid_score =
			ElectionScore { minimal_stake: 10, sum_stake: 10, sum_stake_squared: 100 };
		assert_ok!(MultiBlockSigned::register(RuntimeOrigin::signed(1), invalid_score));

		// unlike `TotalIssuance`, staging_account isn't touched by DAP's ambient issuance drip.
		let staging_before = Balances::free_balance(Dap::staging_account());

		roll_until_matches(|| MultiBlock::current_phase().is_done(), false);

		assert!(
			signed_events_since_last_call()
				.iter()
				.any(|e| matches!(e, SignedEvent::Slashed(..))),
			"expected a Slashed event for the infeasible submission"
		);

		let staging_after = Balances::free_balance(Dap::staging_account());
		assert!(
			staging_after > staging_before,
			"slashed deposit must be routed to the DAP staging account, not burned"
		);
	});
}

#[test]
fn signed_reward_reactivates_dap_buffer_inactive_issuance() {
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// Route DAP's ambient drip away from the buffer, so InactiveIssuance only moves due to
		// our own reward payout.
		pallet_dap::BudgetAllocation::<T>::put(build_budget(&[(staker_reward_key(), 100)]));

		// Seed the buffer with deactivated funds to pay the reward from.
		Balances::mint_into(&Dap::buffer_account(), 1_000).unwrap();
		<Balances as Unbalanced<AccountId>>::deactivate(1_000);

		assert_ok!(rc_client::Pallet::<T>::relay_session_report(
			RuntimeOrigin::root(),
			rc_client::SessionReport {
				end_index: 0,
				validator_points: vec![(1, 10)],
				activation_timestamp: None,
				leftover: false,
			}
		));

		roll_until_matches(|| MultiBlock::current_phase().is_signed(), false);

		let solution = OffchainWorkerMiner::<T>::mine_solution(3, true).unwrap();
		assert_ok!(MultiBlockSigned::register(RuntimeOrigin::signed(1), solution.score));
		for (index, page) in solution.solution_pages.into_iter().enumerate() {
			assert_ok!(MultiBlockSigned::submit_page(
				RuntimeOrigin::signed(1),
				index as u32,
				Some(Box::new(page))
			));
		}

		let inactive_before = pallet_balances::InactiveIssuance::<T>::get();

		roll_until_matches(|| MultiBlock::current_phase().is_done(), false);

		let rewarded = signed_events_since_last_call()
			.into_iter()
			.find_map(|e| match e {
				SignedEvent::Rewarded(_, _, amount) => Some(amount),
				_ => None,
			})
			.expect("expected a Rewarded event for the winning submission");

		let inactive_after = pallet_balances::InactiveIssuance::<T>::get();
		assert_eq!(
			inactive_before - inactive_after,
			rewarded,
			"reactivate() must drop InactiveIssuance by exactly the reward paid"
		);
	});
}

mod session_keys {
	use super::*;
	use crate::ah::mock::{
		Balances, KeyDeposit, LocalQueue, OutgoingMessages, ProxyType, PurgeKeysExecutionCost,
		SetKeysExecutionCost,
	};
	use codec::Encode;
	use frame_support::{assert_noop, pallet_prelude::DispatchError};
	use rc_client::AHStakingInterface;

	type Keys = Vec<u8>;
	type Proof = Vec<u8>;

	/// Helper to create properly encoded session keys and ownership proof.
	fn make_session_keys_and_proof(owner: AccountId) -> (Keys, Proof) {
		let generated = RCSessionKeys::generate(&owner.encode(), None);
		(generated.keys.encode().try_into().unwrap(), generated.proof.encode().try_into().unwrap())
	}

	/// Returns the key-deposit hold for `who`.
	fn key_deposit_hold(who: AccountId) -> Balance {
		<Balances as HoldInspect<AccountId>>::balance_on_hold(
			&rc_client::HoldReason::Keys.into(),
			&who,
		)
	}

	#[test]
	fn set_keys_success() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 1 is a validator with delivery fees configured
			let validator: AccountId = 1;
			let (keys, proof) = make_session_keys_and_proof(validator);
			let keys_raw: Vec<u8> = keys.clone();
			let delivery_fee: u128 = 50;
			XcmDeliveryFee::set(delivery_fee);
			let execution_cost = SetKeysExecutionCost::get();
			let total_fee = delivery_fee + execution_cost;
			let deposit = KeyDeposit::get();
			let balance_before = Balances::free_balance(validator);
			let queue_len_before = LocalQueue::get().unwrap().len();

			// WHEN: Validator sets session keys
			assert_ok!(rc_client::Pallet::<T>::set_keys(
				RuntimeOrigin::signed(validator),
				keys,
				proof,
				None,
			));

			// THEN: FeesPaid event is emitted with total fees (delivery + execution)
			System::assert_has_event(
				rc_client::Event::<T>::FeesPaid { who: validator, fees: total_fee }.into(),
			);

			// AND: Validator's balance is reduced by fees + deposit held
			assert_eq!(Balances::free_balance(validator), balance_before - total_fee - deposit);

			// AND: Key deposit is held
			assert_eq!(key_deposit_hold(validator), deposit);

			// AND: SetKeys message is queued
			let queue = LocalQueue::get().unwrap();
			assert_eq!(queue.len(), queue_len_before + 1);
			assert!(matches!(
				queue.last(),
				Some((_, OutgoingMessages::SetKeys { stash, keys }))
					if *stash == validator && *keys == keys_raw
			));
		});
	}

	#[test]
	fn set_keys_not_validator() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 100 is a nominator, not a validator
			let nominator: AccountId = 100;
			let (keys, proof) = make_session_keys_and_proof(nominator);

			// WHEN: Nominator tries to set keys
			// THEN: NotValidator error is returned
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(nominator),
					keys,
					proof,
					None,
				),
				rc_client::Error::<T>::NotValidator
			);
		});
	}

	#[test]
	fn set_keys_xcm_send_fails() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Validator with sufficient balance and XCM delivery set to fail
			let validator: AccountId = 1;
			let (keys, proof) = make_session_keys_and_proof(validator);
			XcmDeliveryFee::set(100);
			NextRelayDeliveryFails::set(true);

			// WHEN: set_keys fails due to delivery failure
			// THEN: XcmSendFailed error is returned
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys,
					proof,
					None,
				),
				rc_client::Error::<T>::XcmSendFailed
			);
		});
	}

	#[test]
	fn set_keys_invalid_keys() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 1 is a validator with malformed keys
			let validator: AccountId = 1;
			// Cannot be decoded as RCSessionKeys
			let invalid_keys: Keys = vec![0xff, 0xfe, 0xfd].try_into().unwrap();
			let proof: Proof = vec![].try_into().unwrap();

			// WHEN: Validator tries to set invalid keys
			// THEN: InvalidKeys error is returned
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					invalid_keys,
					proof,
					None,
				),
				rc_client::Error::<T>::InvalidKeys
			);
		});
	}

	#[test]
	fn set_keys_invalid_proof() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 1 is a validator with valid keys but wrong proof
			let validator: AccountId = 1;
			// Generate keys for a different account (2) so the proof is invalid for validator (1)
			let (keys, wrong_proof) = make_session_keys_and_proof(2);

			// WHEN: Validator tries to set keys with invalid proof
			// THEN: InvalidProof error is returned
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys,
					wrong_proof,
					None,
				),
				rc_client::Error::<T>::InvalidProof
			);
		});
	}

	#[test]
	fn set_keys_empty_proof() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 1 is a validator with valid keys but empty proof
			let validator: AccountId = 1;
			let (keys, _) = make_session_keys_and_proof(validator);
			let empty_proof: Proof = vec![].try_into().unwrap();

			// WHEN: Validator tries to set keys with empty proof
			// THEN: InvalidProof error is returned
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys,
					empty_proof,
					None,
				),
				rc_client::Error::<T>::InvalidProof
			);
		});
	}

	#[test]
	fn set_keys_malformed_proof() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 1 is a validator with valid keys but malformed proof
			let validator: AccountId = 1;
			let (keys, _) = make_session_keys_and_proof(validator);
			let malformed_proof: Proof = vec![0xde, 0xad, 0xbe, 0xef].try_into().unwrap();

			// WHEN: Validator tries to set keys with malformed proof
			// THEN: InvalidProof error is returned
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys,
					malformed_proof,
					None,
				),
				rc_client::Error::<T>::InvalidProof
			);
		});
	}

	#[test]
	fn set_keys_via_staking_proxy() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 1 is a validator (stash), account 99 is the proxy
			let stash: AccountId = 1;
			let proxy: AccountId = 99;
			let (keys, proof) = make_session_keys_and_proof(stash);

			// Fund the proxy account so it can pay for proxy deposit
			assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), proxy, 1000));

			// Set up proxy relationship: proxy can act on behalf of stash for Staking calls
			assert_ok!(pallet_proxy::Pallet::<T>::add_proxy(
				RuntimeOrigin::signed(stash),
				proxy,
				ProxyType::Staking,
				0, // no delay
			));

			// WHEN: Proxy calls set_keys on behalf of stash via Proxy::proxy()
			let set_keys_call = RuntimeCall::RcClient(rc_client::Call::set_keys {
				keys: keys.clone(),
				proof,
				max_delivery_and_remote_execution_fee: None,
			});

			// THEN: The call succeeds (keys forwarded to RC via XCM)
			assert_ok!(pallet_proxy::Pallet::<T>::proxy(
				RuntimeOrigin::signed(proxy),
				stash,
				None, // no force_proxy_type
				Box::new(set_keys_call),
			));

			// WHEN: Proxy calls purge_keys on behalf of stash
			let purge_keys_call = RuntimeCall::RcClient(rc_client::Call::purge_keys {
				max_delivery_and_remote_execution_fee: None,
			});

			// THEN: The call succeeds
			assert_ok!(pallet_proxy::Pallet::<T>::proxy(
				RuntimeOrigin::signed(proxy),
				stash,
				None,
				Box::new(purge_keys_call),
			));
		});
	}

	#[test]
	fn set_keys_fails_with_insufficient_balance() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Validator with insufficient balance to pay total fees
			let validator: AccountId = 1;
			let (keys, proof) = make_session_keys_and_proof(validator);
			XcmDeliveryFee::set(Balances::free_balance(validator) + 1);

			// WHEN: Validator tries to set keys
			// THEN: XcmSendFailed error is returned (fee charging failure causes XCM send to fail)
			assert_noop!(
				rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys,
					proof,
					None,
				),
				rc_client::Error::<T>::XcmSendFailed
			);
		});
	}

	#[test]
	fn set_keys_max_fee_scenarios() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			let validator: AccountId = 1;
			let (keys, proof) = make_session_keys_and_proof(validator);
			let delivery_fee: u128 = 50;
			XcmDeliveryFee::set(delivery_fee);
			let execution_cost = SetKeysExecutionCost::get();
			let total_fee = delivery_fee + execution_cost;
			let deposit = KeyDeposit::get();
			let balance_before = Balances::free_balance(validator);

			// max_fee > total: succeeds, charges total fee + deposit
			hypothetically!({
				assert_ok!(rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys.clone(),
					proof.clone(),
					Some(total_fee + 100),
				));
				assert_eq!(Balances::free_balance(validator), balance_before - total_fee - deposit);
			});

			// max_fee == total: succeeds (max_fee only caps XCM fees, not the deposit)
			hypothetically!({
				assert_ok!(rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys.clone(),
					proof.clone(),
					Some(total_fee),
				));
				assert_eq!(Balances::free_balance(validator), balance_before - total_fee - deposit);
			});

			// max_fee < total: fails with FeesExceededMax
			hypothetically!({
				assert_noop!(
					rc_client::Pallet::<T>::set_keys(
						RuntimeOrigin::signed(validator),
						keys.clone(),
						proof.clone(),
						Some(total_fee - 1),
					),
					rc_client::Error::<T>::FeesExceededMax
				);
			});

			// max_fee = 0, total = 0: succeeds (both delivery and execution cost are 0)
			hypothetically!({
				XcmDeliveryFee::set(0);
				SetKeysExecutionCost::set(0);
				assert_ok!(rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys.clone(),
					proof.clone(),
					Some(0),
				));
				// Only deposit is held, no XCM fees
				assert_eq!(Balances::free_balance(validator), balance_before - deposit);
			});
		});
	}

	#[test]
	fn purge_keys_success() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 3 is a validator that has set keys (deposit held)
			let validator: AccountId = 3;
			let (keys, proof) = make_session_keys_and_proof(validator);
			assert_ok!(rc_client::Pallet::<T>::set_keys(
				RuntimeOrigin::signed(validator),
				keys,
				proof,
				None,
			));
			assert_eq!(key_deposit_hold(validator), KeyDeposit::get());

			let delivery_fee: u128 = 50;
			XcmDeliveryFee::set(delivery_fee);
			let execution_cost = PurgeKeysExecutionCost::get();
			let total_fee = delivery_fee + execution_cost;
			let balance_before = Balances::free_balance(validator);
			let queue_len_before = LocalQueue::get().unwrap().len();

			// WHEN: Validator purges session keys
			assert_ok!(rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(validator), None,));

			// THEN: FeesPaid event is emitted with total fees (delivery + execution)
			System::assert_has_event(
				rc_client::Event::<T>::FeesPaid { who: validator, fees: total_fee }.into(),
			);

			// AND: Deposit is released, balance only reduced by purge fee
			let deposit = KeyDeposit::get();
			assert_eq!(Balances::free_balance(validator), balance_before - total_fee + deposit);

			// AND: Key deposit is released
			assert_eq!(key_deposit_hold(validator), 0);

			// AND: PurgeKeys message is queued
			let queue = LocalQueue::get().unwrap();
			assert_eq!(queue.len(), queue_len_before + 1);
			assert!(matches!(
				queue.last(),
				Some((_, OutgoingMessages::PurgeKeys { stash })) if *stash == validator
			));
		});
	}

	/// Test that purge_keys can be called by any account.
	///
	/// Unlike set_keys (which requires validator status), purge_keys allows anyone to call it.
	/// This matches the original pallet-session behavior and enables validators to purge their
	/// keys after chilling. The RC will reject with NoKeys if no keys exist.
	#[test]
	fn purge_keys_allowed_for_any_account() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 101 is a nominator, not a validator
			let nominator: AccountId = 101;

			// WHEN: Non-validator calls purge_keys
			// THEN: Call succeeds - XCM is sent to RC (RC will validate if keys exist)
			assert_ok!(rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(nominator), None,));
		});
	}

	/// Test that a chilled validator can still purge their session keys.
	#[test]
	fn purge_keys_after_chilling() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account 3 is initially a validator
			let validator: AccountId = 3;
			assert!(Staking::is_validator(&validator), "Account 3 should be a validator");

			// WHEN: Validator chills (stops being a validator)
			assert_ok!(Staking::chill(RuntimeOrigin::signed(validator)));
			assert!(
				!Staking::is_validator(&validator),
				"Account 3 should no longer be a validator"
			);

			// THEN: Chilled account can still purge their session keys
			assert_ok!(rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(validator), None,));
		});
	}

	#[test]
	fn purge_keys_xcm_send_fails() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Validator with XCM delivery set to fail
			let validator: AccountId = 5;
			XcmDeliveryFee::set(100);
			NextRelayDeliveryFails::set(true);

			// WHEN: Validator purges session keys
			// THEN: XcmSendFailed error is returned
			assert_noop!(
				rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(validator), None),
				rc_client::Error::<T>::XcmSendFailed
			);
		});
	}

	#[test]
	fn purge_keys_fails_with_insufficient_balance() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			// GIVEN: Account with insufficient balance to pay total fees
			let account: AccountId = 3;
			XcmDeliveryFee::set(Balances::free_balance(account) + 1);

			// WHEN: Account tries to purge keys
			// THEN: XcmSendFailed error is returned (fee charging failure causes XCM send to fail)
			assert_noop!(
				rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(account), None),
				rc_client::Error::<T>::XcmSendFailed
			);
		});
	}

	#[test]
	fn purge_keys_max_fee_scenarios() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			let account: AccountId = 3;
			let delivery_fee: u128 = 50;
			XcmDeliveryFee::set(delivery_fee);
			let execution_cost = PurgeKeysExecutionCost::get();
			let total_fee = delivery_fee + execution_cost;
			let balance_before = Balances::free_balance(account);

			// max_fee > total: succeeds
			hypothetically!({
				assert_ok!(rc_client::Pallet::<T>::purge_keys(
					RuntimeOrigin::signed(account),
					Some(total_fee + 100),
				));
				assert_eq!(Balances::free_balance(account), balance_before - total_fee);
			});

			// max_fee == total: succeeds
			hypothetically!({
				assert_ok!(rc_client::Pallet::<T>::purge_keys(
					RuntimeOrigin::signed(account),
					Some(total_fee),
				));
				assert_eq!(Balances::free_balance(account), balance_before - total_fee);
			});

			// max_fee < total: fails with FeesExceededMax
			hypothetically!({
				assert_noop!(
					rc_client::Pallet::<T>::purge_keys(
						RuntimeOrigin::signed(account),
						Some(total_fee - 1),
					),
					rc_client::Error::<T>::FeesExceededMax
				);
			});
		});
	}

	#[test]
	fn set_keys_deposit_lifecycle() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			let validator: AccountId = 1;
			let deposit = KeyDeposit::get();
			let set_fees = XcmDeliveryFee::get() + SetKeysExecutionCost::get();
			let purge_fees = XcmDeliveryFee::get() + PurgeKeysExecutionCost::get();

			// First set_keys: deposit held
			let (keys, proof) = make_session_keys_and_proof(validator);
			let balance_before = Balances::free_balance(validator);
			assert_ok!(rc_client::Pallet::<T>::set_keys(
				RuntimeOrigin::signed(validator),
				keys,
				proof,
				None,
			));
			assert_eq!(key_deposit_hold(validator), deposit);
			assert_eq!(Balances::free_balance(validator), balance_before - set_fees - deposit);

			// Second set_keys: no additional deposit
			let balance_after_first = Balances::free_balance(validator);
			let (keys2, proof2) = make_session_keys_and_proof(validator);
			assert_ok!(rc_client::Pallet::<T>::set_keys(
				RuntimeOrigin::signed(validator),
				keys2,
				proof2,
				None,
			));
			assert_eq!(Balances::free_balance(validator), balance_after_first - set_fees);

			// Purge with XCM failure: deposit NOT released (transactional rollback)
			let balance_before_failed_purge = Balances::free_balance(validator);
			hypothetically!({
				NextRelayDeliveryFails::set(true);
				assert_noop!(
					rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(validator), None),
					rc_client::Error::<T>::XcmSendFailed
				);
				assert_eq!(key_deposit_hold(validator), deposit);
				assert_eq!(Balances::free_balance(validator), balance_before_failed_purge);
			});

			// Successful purge: deposit released
			let balance_before_purge = Balances::free_balance(validator);
			assert_ok!(rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(validator), None));
			assert_eq!(key_deposit_hold(validator), 0);
			assert_eq!(
				Balances::free_balance(validator),
				balance_before_purge - purge_fees + deposit
			);
		});
	}

	#[test]
	fn set_keys_deposit_edge_cases() {
		ExtBuilder::default().local_queue().build().execute_with(|| {
			let validator: AccountId = 1;
			let set_fees = XcmDeliveryFee::get() + SetKeysExecutionCost::get();

			// Invalid keys: deposit not charged
			hypothetically!({
				let balance_before = Balances::free_balance(validator);
				assert_noop!(
					rc_client::Pallet::<T>::set_keys(
						RuntimeOrigin::signed(validator),
						vec![0xff, 0xfe, 0xfd],
						vec![],
						None,
					),
					rc_client::Error::<T>::InvalidKeys
				);
				assert_eq!(key_deposit_hold(validator), 0);
				assert_eq!(Balances::free_balance(validator), balance_before);
			});

			// Insufficient balance for deposit
			hypothetically!({
				let (keys, proof) = make_session_keys_and_proof(validator);
				KeyDeposit::set(Balances::free_balance(validator) + 1);
				assert_noop!(
					rc_client::Pallet::<T>::set_keys(
						RuntimeOrigin::signed(validator),
						keys,
						proof,
						None,
					),
					DispatchError::Token(frame::runtime::prelude::TokenError::FundsUnavailable)
				);
			});

			// Zero deposit: set + purge lifecycle works, no hold placed
			hypothetically!({
				KeyDeposit::set(0);
				let (keys, proof) = make_session_keys_and_proof(validator);
				let balance_before = Balances::free_balance(validator);

				assert_ok!(rc_client::Pallet::<T>::set_keys(
					RuntimeOrigin::signed(validator),
					keys,
					proof,
					None,
				));
				assert_eq!(key_deposit_hold(validator), 0);
				assert_eq!(Balances::free_balance(validator), balance_before - set_fees);

				assert_ok!(rc_client::Pallet::<T>::purge_keys(
					RuntimeOrigin::signed(validator),
					None,
				));
				assert_eq!(key_deposit_hold(validator), 0);
			});

			// Purge without prior set_keys: no deposit to release, only XCM fees
			hypothetically!({
				let other: AccountId = 3;
				assert_eq!(key_deposit_hold(other), 0);
				let balance_before = Balances::free_balance(other);
				let purge_fees = XcmDeliveryFee::get() + PurgeKeysExecutionCost::get();

				assert_ok!(rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(other), None,));
				assert_eq!(Balances::free_balance(other), balance_before - purge_fees);
			});
		});
	}

	#[test]
	fn purge_keys_on_rc_does_not_release_ah_deposit() {
		shared::put_ah_state(ExtBuilder::default().build());
		shared::put_rc_state(rc::ExtBuilder::default().session_keys(vec![1]).build());

		let validator: AccountId = 1;
		let deposit = KeyDeposit::get();

		// GIVEN: Validator sets keys on AH (deposit is held).
		shared::in_ah(|| {
			let (keys, proof) = make_session_keys_and_proof(validator);
			assert_ok!(rc_client::Pallet::<T>::set_keys(
				RuntimeOrigin::signed(validator),
				keys,
				proof,
				None,
			));
			assert_eq!(key_deposit_hold(validator), deposit);
		});

		// WHEN: Keys are purged directly on RC (bypassing AH).
		shared::in_rc(|| {
			assert_ok!(pallet_session::Pallet::<rc::Runtime>::purge_keys(
				rc::RuntimeOrigin::signed(validator),
			));
			let next_keys = pallet_session::NextKeys::<rc::Runtime>::get(validator);
			assert!(next_keys.is_none(), "Keys should be purged on RC");
		});

		// THEN: The AH deposit is still held — only AH's purge_keys releases it.
		shared::in_ah(|| {
			assert_eq!(key_deposit_hold(validator), deposit);
		});
	}

	/// End-to-end test: set keys on AssetHub, verify on RelayChain, then purge and verify.
	#[test]
	fn set_and_purge_keys_e2e() {
		// Set up both AH and RC states (no local_queue, so XCM messages flow through)
		shared::put_ah_state(ExtBuilder::default().build());
		shared::put_rc_state(rc::ExtBuilder::default().session_keys(vec![1]).build());

		let validator: AccountId = 1;

		// Generate valid keys and proof for AH validation (must match AHSessionKeys structure)
		let (encoded_keys, proof) = make_session_keys_and_proof(validator);

		// WHEN: Validator sets keys on AH
		shared::in_ah(|| {
			assert_ok!(rc_client::Pallet::<T>::set_keys(
				RuntimeOrigin::signed(validator),
				encoded_keys.clone(),
				proof.clone(),
				None,
			));
		});

		// THEN: Keys are registered on RC (RC uses UintAuthorityId directly)
		shared::in_rc(|| {
			let next_keys = pallet_session::NextKeys::<rc::Runtime>::get(validator);
			assert!(next_keys.is_some(), "Keys should be set on RC");
		});

		// WHEN: Validator purges keys on AH
		shared::in_ah(|| {
			assert_ok!(rc_client::Pallet::<T>::purge_keys(RuntimeOrigin::signed(validator), None,));
		});

		// THEN: Keys are purged on RC
		shared::in_rc(|| {
			let next_keys = pallet_session::NextKeys::<rc::Runtime>::get(validator);
			assert!(next_keys.is_none(), "Keys should be purged on RC");
		});
	}
}

/// Mirrors the DAP drip setup migration logic.
struct LastInflationTs;
impl frame_support::traits::Get<u64> for LastInflationTs {
	fn get() -> u64 {
		ActiveEra::<T>::get().and_then(|e| e.start).unwrap_or(0)
	}
}

/// Provides the initial DAP budget (85 staker / 15 buffer / 0 incentive) for the migration.
struct InitialBudget;
impl frame_support::traits::Get<pallet_dap::BudgetAllocationMap> for InitialBudget {
	fn get() -> pallet_dap::BudgetAllocationMap {
		build_budget(&[(staker_reward_key(), 85), (buffer_key(), 15)])
	}
}

/// E2E: legacy → DAP (no incentive) → DAP (with validator incentive).
///
/// Budget splits are staker/incentive/treasury throughout:
/// - Legacy (minting): 85/0/15
/// - DAP without incentive: 85/0/15
/// - DAP with incentive: 65/20/15
///
/// Era 0: legacy 85/0/15.
/// Era 1: DAP 85/0/15. First DAP era, no incentive.
/// Era 2: governance sets incentive config + budget 65/20/15. Transition era.
/// Era 3: full 65/20/15 — weights picked up from era 2 election.
#[test]
fn legacy_to_dap_era_payout_e2e() {
	// Phase 1: Legacy mode
	UseLegacyEraPayout::set(true);
	LegacyStakerPercent::set(85);
	ExtBuilder::default().local_queue().build().execute_with(|| {
		// Validator 5: own=100, nominator 110=100, total=200 (50/50 stake split).
		let alice: AccountId = 5; // validator
		let bob: AccountId = 110; // nominator backing alice

		for v in [3u64, 5, 6, 8] {
			staking_async::Payee::<T>::insert(v, staking_async::RewardDestination::Stash);
		}
		// Also set nominator payee to Stash so rewards don't change stake between eras.
		staking_async::Payee::<T>::insert(bob, staking_async::RewardDestination::Stash);

		let set_reward_points = |era: u32| {
			staking_async::ErasRewardPoints::<T>::mutate(era, |points| {
				points.total = 4;
				for v in [3, 5, 6, 8] {
					points.individual.try_insert(v, 1).unwrap();
				}
			});
			// Mirror the denominator that `reward_active_era` maintains in production, so the
			// weighted-points incentive share resolves (it is otherwise zero here because this
			// helper writes `ErasRewardPoints` directly). Sum = Σ weight · points.
			let sum = [3, 5, 6, 8].into_iter().fold(0u128, |acc, v| {
				let weight = staking_async::ErasValidatorIncentiveWeight::<T>::get(era, v)
					.unwrap_or_default();
				acc + weight // each validator has exactly 1 point
			});
			staking_async::ErasSumWeightedPoints::<T>::insert(era, sum);
		};

		// -- Era 0 (legacy 85/0/15) --

		roll_until_next_active(1);
		assert_eq!(Rotator::<T>::active_era(), 1);
		assert_eq!(DisableMintingGuard::<T>::get(), None);
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::SessionRotated { starting_session: 2, active_era: 0, planned_era: 1 },
				StakingEvent::PagedElectionProceeded { page: 2, result: Ok(4) },
				StakingEvent::PagedElectionProceeded { page: 1, result: Ok(0) },
				StakingEvent::PagedElectionProceeded { page: 0, result: Ok(0) },
				StakingEvent::SessionRotated { starting_session: 3, active_era: 0, planned_era: 1 },
				StakingEvent::SessionRotated { starting_session: 4, active_era: 0, planned_era: 1 },
				StakingEvent::SessionRotated { starting_session: 5, active_era: 0, planned_era: 1 },
				StakingEvent::SessionRotated { starting_session: 6, active_era: 0, planned_era: 1 },
				// 27 blocks × 12_000ms = 324_000ms. 85% = 275_400 staker, 15% = 48_600 treasury.
				StakingEvent::EraPaid { era_index: 0, validator_payout: 275400, remainder: 48600 },
				StakingEvent::SessionRotated { starting_session: 7, active_era: 1, planned_era: 2 },
			]
		);

		// -- Switch to DAP 85/0/15 mid-era via runtime upgrade migration --
		// Emulates what AH will do in a real upgrade: `MigrateV1ToV2` seeds
		// `BudgetAllocation`, backfills a catch-up drip from `ActiveEra.start` to `now`,
		// then sets `LastIssuanceTimestamp = now` so regular drips resume from here.

		UseLegacyEraPayout::set(false);
		// ED to keep the general staker pot alive for the catch-up drip.
		let general_staker =
			SequentialTest::pot_account(RewardPot::General(RewardKind::StakerRewards));
		Balances::mint_into(&general_staker, 1).unwrap();

		// Roll 5 blocks into era 1 so the migration has a non-trivial elapsed window to
		// backfill: `elapsed = now - ActiveEra.start`.
		let era_1_start = ActiveEra::<T>::get().and_then(|e| e.start).unwrap();
		roll_many(5);
		let before_migration = MockTime::get();
		let expected_elapsed = before_migration - era_1_start;
		assert_eq!(expected_elapsed, 5 * BLOCK_TIME);

		// Reset `LastIssuanceTimestamp` to 0 so the migration sees a fresh DAP install
		pallet_dap::LastIssuanceTimestamp::<T>::kill();
		let pot_before_migration = Balances::total_balance(&general_staker);

		// Run the migration.
		use frame_support::traits::UncheckedOnRuntimeUpgrade;
		let _ = pallet_dap::migrations::InnerMigrateV1ToV2::<
			T,
			LastInflationTs,
			InitialBudget,
			frame_support::traits::ConstU64<{ 6 * 12_000 }>,
		>::on_runtime_upgrade();

		// Migration seeded the budget and anchored `LastIssuanceTimestamp` at `now`.
		assert!(!pallet_dap::BudgetAllocation::<T>::get().is_empty());
		assert_eq!(pallet_dap::LastIssuanceTimestamp::<T>::get(), before_migration);

		// Catch-up drip minted 1 token/ms × elapsed; 85% → staker pot.
		let expected_backfill_to_staker =
			Perbill::from_percent(85).mul_floor(expected_elapsed as u128);
		assert_eq!(
			Balances::total_balance(&general_staker) - pot_before_migration,
			expected_backfill_to_staker
		);

		// Regular drips resume per block from here: one `roll_next` → +1 block of drip.
		let pot_after_migration = Balances::total_balance(&general_staker);
		roll_next();
		let expected_per_block_drip = Perbill::from_percent(85).mul_floor(BLOCK_TIME as u128);
		assert_eq!(
			Balances::total_balance(&general_staker) - pot_after_migration,
			expected_per_block_drip
		);

		// Switching to DAP mode alone doesn't flip the guard; it only flips when the
		// first DAP era ends (snapshotted into an era pot).
		assert!(DisableMintingGuard::<T>::get().is_none());

		// -- Era 1 (DAP 85/0/15, no incentive) --

		set_reward_points(1);
		let _ = staking_events_since_last_call();
		roll_until_next_active(7);
		assert_eq!(Rotator::<T>::active_era(), 2);
		// Era 1 just ended in DAP mode → guard flips to Some(1).
		assert_eq!(DisableMintingGuard::<T>::get(), Some(1));

		// Same 85% split as legacy, same era duration → identical total staker reward.
		assert_eq!(ErasValidatorReward::<T>::get(1).unwrap(), 275400);
		assert_eq!(ErasValidatorIncentiveBudget::<T>::get(1), 0);

		// Payout alice (validator 5): 4 validators with equal points, 0% commission.
		// Per-validator share = 275400 / 4 = 68850.
		// Alice has 50% own stake → validator reward = 34425, nominator reward = 34425.
		let pre_issuance = Balances::total_issuance();
		let alice_before = Balances::total_balance(&alice);
		let bob_before = Balances::total_balance(&bob);
		let _ = staking_events_since_last_call();
		assert_ok!(staking_async::Pallet::<T>::payout_stakers(
			RuntimeOrigin::signed(999),
			alice,
			1,
		));
		assert_eq!(Balances::total_issuance(), pre_issuance); // DAP: no new mint

		let era1_alice = Balances::total_balance(&alice) - alice_before;
		let era1_bob = Balances::total_balance(&bob) - bob_before;
		assert_eq!(era1_alice, 34425);
		assert_eq!(era1_bob, 34425);
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::PayoutStarted {
					era_index: 1,
					validator_stash: alice,
					page: 0,
					next: None,
				},
				StakingEvent::Rewarded {
					stash: alice,
					dest: staking_async::RewardDestination::Stash,
					amount: era1_alice,
				},
				StakingEvent::Rewarded {
					stash: bob,
					dest: staking_async::RewardDestination::Stash,
					amount: era1_bob,
				},
			]
		);

		// -- Governance: enable incentive, budget goes to 65/20/15 --
		// Era 2's election already happened (during era 1) without config so era 2 won't
		// have incentive weights. Era 3's election runs during era 2 and picks them up.

		assert_ok!(Staking::set_validator_self_stake_incentive_config(
			RuntimeOrigin::root(),
			staking_async::ConfigOp::Set(30),   // OptimumSelfStake
			staking_async::ConfigOp::Set(1000), // HardCapSelfStake
			staking_async::ConfigOp::Set(Perbill::from_rational(1u32, 2u32)),
		));
		pallet_dap::BudgetAllocation::<T>::put(build_budget(&[
			(staker_reward_key(), 65),
			(validator_incentive_key(), 20),
			(buffer_key(), 15),
		]));
		Balances::mint_into(
			&SequentialTest::pot_account(RewardPot::General(RewardKind::ValidatorSelfStake)),
			1,
		)
		.unwrap();

		// -- Era 2 (transition — no incentive weights yet, skip payout) --

		set_reward_points(2);
		let _ = staking_events_since_last_call();
		roll_until_next_active(13);
		assert_eq!(Rotator::<T>::active_era(), 3);

		// -- Era 3 (full 65/20/15) --

		set_reward_points(3);
		let _ = staking_events_since_last_call();
		roll_until_next_active(19);
		assert_eq!(Rotator::<T>::active_era(), 4);

		assert_eq!(ErasValidatorReward::<T>::get(3).unwrap(), 210600); // 65%
		assert_eq!(ErasValidatorIncentiveBudget::<T>::get(3), 64800); // 20%

		// Payout alice for era 3.
		// Staker share per validator = 210600 / 4 = 52650. Split 50/50 → 26325 each.
		// Incentive per validator = 64800 / 4 = 16200. Goes entirely to alice.
		let pre_issuance = Balances::total_issuance();
		let alice_before = Balances::total_balance(&alice);
		let bob_before = Balances::total_balance(&bob);
		let _ = staking_events_since_last_call();
		assert_ok!(staking_async::Pallet::<T>::payout_stakers(
			RuntimeOrigin::signed(999),
			alice,
			3,
		));
		assert_eq!(Balances::total_issuance(), pre_issuance); // DAP: no new mint

		let era3_alice_staker = 26325u128; // 52650 * 50%
		let era3_alice_incentive = 16200u128; // 64800 / 4
		let era3_bob = 26325u128; // 52650 * 50%
		assert_eq!(
			Balances::total_balance(&alice) - alice_before,
			era3_alice_staker + era3_alice_incentive
		);
		assert_eq!(Balances::total_balance(&bob) - bob_before, era3_bob);
		assert_eq!(
			staking_events_since_last_call(),
			vec![
				StakingEvent::PayoutStarted {
					era_index: 3,
					validator_stash: alice,
					page: 0,
					next: None,
				},
				StakingEvent::ValidatorIncentivePaid {
					era: 3,
					validator_stash: alice,
					dest: staking_async::RewardDestination::Stash,
					amount: era3_alice_incentive,
				},
				StakingEvent::Rewarded {
					stash: alice,
					dest: staking_async::RewardDestination::Stash,
					amount: era3_alice_staker,
				},
				StakingEvent::Rewarded {
					stash: bob,
					dest: staking_async::RewardDestination::Stash,
					amount: era3_bob,
				},
			]
		);

		// -- Key comparisons across eras --
		// All eras have identical total issuance rate (324_000 tokens/era).

		// 1) Legacy and DAP produce identical staker rewards with same budget. legacy era 0 staker
		//    payout = DAP era 1 staker payout = 275400.

		// 2) Validator reward increases with incentive. era 1: alice got 34425 (staker only). era
		//    3: alice got 26325 + 16200 = 42525 (staker + incentive). +23.5% increase.
		assert_eq!(era1_alice, 34425);
		assert_eq!(era3_alice_staker + era3_alice_incentive, 42525);
		assert!(era3_alice_staker + era3_alice_incentive > era1_alice);

		// 3) Nominator reward decreases (staker budget dropped 85→65%). era 1: bob got 34425. era
		//    3: bob got 26325. -23.5% decrease.
		assert_eq!(era1_bob, 34425);
		assert_eq!(era3_bob, 26325);
		assert!(era3_bob < era1_bob);

		// 4) Treasury/buffer portion stays at 15% across all phases. legacy: remainder=48600 (15%
		//    of 324000). DAP: buffer gets 15% of each era's issuance.

		// 5) Total inflation is the same — just distributed differently. era 1 total per-validator:
		//    34425 + 34425 = 68850. era 3 total per-validator: 42525 + 26325 = 68850. Same!
		assert_eq!(era1_alice + era1_bob, era3_alice_staker + era3_alice_incentive + era3_bob);
	});
}
