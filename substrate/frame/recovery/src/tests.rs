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

//! Tests for the module.

use crate::{mock::*, Call as RecoveryCall, *};
use frame::{prelude::fungible::UnbalancedHold, testing_prelude::*};
use pallet_balances::Call as BalancesCall;
use sp_runtime::{DispatchError, TokenError};

use Test as T;

#[test]
fn basic_flow_works() {
	new_test_ext().execute_with(|| {
		// Alice configures one friend group with Bob, Charlie and Dave
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg]));

		// Bob initiates the attempt (auto-approves himself).
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		// Charlie approves to reach the threshold.
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));

		// Eve finishes the attempt too early (10 inheritance delay)
		assert_noop!(
			Recovery::finish_attempt(signed(EVE), ALICE, 0),
			Error::<T>::NotYetInheritable
		);

		// Advance the block number to 11
		System::set_block_number(11);

		// Eve finishes the attempt
		assert_ok!(Recovery::finish_attempt(signed(EVE), ALICE, 0));

		// Eve finishes the attempt and Ferdie should be the inheritor
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert_eq!(Recovery::inheritance(FERDIE), vec![ALICE]);

		assert_ok!(Recovery::control_inherited_account(
			signed(FERDIE),
			ALICE,
			Box::new(BalancesCall::transfer_all { dest: FERDIE, keep_alive: false }.into())
		));

		// Alice will still have the friend group deposit
		assert_fg_deposit(ALICE, 63);
		// but otherwise only ED
		assert_eq!(<Test as Config>::Currency::total_balance(&ALICE), 63 + 1);
		// Ferdie will have the balance
		assert_eq!(<Test as Config>::Currency::total_balance(&FERDIE), 2 * START_BALANCE - 64);
	});
}

/// Setting multiple friend groups works.
#[test]
fn set_friend_groups_multiple_works() {
	new_test_ext().execute_with(|| {
		// Alice configures two friend groups with Bob, Charlie and Dave
		let fg1 = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		let fg2 = FriendGroupOf::<T> {
			friends: friends([CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		let friend_groups = vec![fg1, fg2];

		// Alice configures two friend groups
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), friend_groups));
		// Deposit taken for both friend groups
		assert_fg_deposit(ALICE, 112);
	});
}

/// Setting a friend group with too many `friends_needed` fails.
#[test]
fn set_friend_groups_too_many_friends_needed_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 4,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		let friend_groups = vec![fg];

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), friend_groups),
			Error::<T>::TooManyFriendsNeeded
		);
	});
}

/// Setting a friend group with no friends fails.
#[test]
fn set_friend_groups_no_friends_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([]),
			friends_needed: 0,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_noop!(Recovery::set_friend_groups(signed(ALICE), vec![fg]), Error::<T>::NoFriends);
	});
}

/// Setting a friend group with `friends_needed` of zero fails.
#[test]
fn set_friend_groups_zero_friends_needed_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 0,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg]),
			Error::<T>::NoFriendsNeeded
		);
	});
}

/// Setting a friend group with `cancel_delay` of zero fails.
#[test]
fn set_friend_groups_zero_cancel_delay_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 0,
		};

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg]),
			Error::<T>::NoCancelDelay
		);
	});
}

/// Setting a friend group with unsorted friends fails.
#[test]
fn set_friend_groups_unsorted_friends_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([CHARLIE, BOB, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg]),
			Error::<T>::FriendsNotSortedOrUnique
		);
	});
}

/// Setting a friend group with duplicate friends fails.
#[test]
fn set_friend_groups_duplicate_friends_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, BOB, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg]),
			Error::<T>::FriendsNotSortedOrUnique
		);
	});
}

/// Setting two friend groups with the same set of friends fails.
#[test]
fn set_friend_groups_duplicate_groups_fails() {
	new_test_ext().execute_with(|| {
		let fg1 = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		let fg2 = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 1,
			inheritor: EVE,
			inheritance_delay: 5,
			inheritance_priority: 1,
			cancel_delay: 5,
		};

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg1, fg2]),
			Error::<T>::DuplicateFriendGroups
		);
	});
}

/// Can remove all friend groups also with deposits.
#[test]
fn set_friend_groups_remove_works() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg.clone()]));
		assert_eq!(Recovery::friend_groups(ALICE), vec![fg.clone()]);
		assert_fg_deposit(ALICE, 63);

		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![]));
		assert_eq!(Recovery::friend_groups(ALICE), vec![]);
		assert_fg_deposit(ALICE, 0);

		// re-add
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg.clone()]));
		assert_eq!(Recovery::friend_groups(ALICE), vec![fg.clone()]);
		assert_fg_deposit(ALICE, 63);

		// re-remove
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![]));
		assert_eq!(Recovery::friend_groups(ALICE), vec![]);
		assert_fg_deposit(ALICE, 0);
	});
}

/// Cannot change friend groups if there are ongoing attempts.
#[test]
fn set_friend_groups_ongoing_attempt_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg.clone()]));
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg]),
			Error::<T>::HasOngoingAttempts
		);
	});
}

/// The lost account cannot include themselves in their own friend group.
#[test]
fn set_friend_groups_lost_account_in_group_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([ALICE, BOB, CHARLIE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), vec![fg]),
			Error::<T>::LostAccountInFriendGroup
		);
	});
}

/// Maximum number of friend groups (10) can be set.
#[test]
fn set_friend_groups_max_groups_works() {
	new_test_ext().execute_with(|| {
		let friend_groups: Vec<_> = (0..10u8)
			.map(|i| FriendGroupOf::<T> {
				friends: friends([BOB, 10 + i as u64]),
				friends_needed: 1,
				inheritor: FERDIE,
				inheritance_delay: 10,
				inheritance_priority: i as u32,
				cancel_delay: 10,
			})
			.collect();

		assert_ok!(Recovery::set_friend_groups(signed(ALICE), friend_groups));
		assert_eq!(Recovery::friend_groups(ALICE).len(), 10);
	});
}

/// Cannot initiate more than one attempt per friend group.
#[test]
fn initiate_attempt_multiple_fails() {
	new_test_ext().execute_with(|| {
		let fg = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg]));

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_noop!(
			Recovery::initiate_attempt(signed(BOB), ALICE, 0),
			Error::<T>::AlreadyInitiated
		);
		assert_noop!(
			Recovery::initiate_attempt(signed(CHARLIE), ALICE, 0),
			Error::<T>::AlreadyInitiated
		);
	});
}

/// Cannot initiate without any friend groups.
#[test]
fn initiate_attempt_no_friend_groups_fails() {
	new_test_ext().execute_with(|| {
		assert_noop!(Recovery::initiate_attempt(signed(BOB), ALICE, 0), Error::<T>::NoFriendGroups);
	});
}

/// Can initiate attempts for different friend groups.
#[test]
fn initiate_attempt_different_friend_groups_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, DAVE], [BOB, CHARLIE]]);

		frame_system::Pallet::<T>::set_block_number(10);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_security_deposit(BOB, SECURITY_DEPOSIT);

		hypothetically!({
			assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 1));
			assert_security_deposit(BOB, SECURITY_DEPOSIT * 2);
		});

		assert_ok!(Recovery::initiate_attempt(signed(CHARLIE), ALICE, 1));
		assert_security_deposit(CHARLIE, SECURITY_DEPOSIT);

		assert_eq!(
			Recovery::attempts(ALICE),
			vec![
				(
					fg([BOB, DAVE]),
					AttemptOf::<T> {
						friend_group_index: 0,
						initiator: BOB.into(),
						init_block: 10,
						last_approval_block: 10,
						approvals: ApprovalBitfield::default().with_bits([0]).unwrap(),
					}
				),
				(
					fg([BOB, CHARLIE]),
					AttemptOf::<T> {
						friend_group_index: 1,
						initiator: CHARLIE.into(),
						init_block: 10,
						last_approval_block: 10,
						approvals: ApprovalBitfield::default().with_bits([1]).unwrap(),
					}
				)
			]
		);
	});
}

/// A non-friend cannot initiate a recovery attempt.
#[test]
fn initiate_attempt_not_friend_fails() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE]]);

		assert_noop!(Recovery::initiate_attempt(signed(EVE), ALICE, 0), Error::<T>::NotFriend);
	});
}

/// Cannot initiate with an invalid friend group index.
#[test]
fn initiate_attempt_not_friend_group_fails() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE]]);

		assert_noop!(Recovery::initiate_attempt(signed(BOB), ALICE, 5), Error::<T>::NotFriendGroup);
	});
}

/// Initiating an attempt emits both `AttemptInitiated` and `AttemptApproved` for the initiator,
/// since the initiator is auto-approved.
#[test]
fn initiate_attempt_emits_approved_for_initiator() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		// The last two events should be `AttemptInitiated` followed by `AttemptApproved`.
		let events = frame_system::Pallet::<T>::events();
		let tail = &events[events.len() - 2..];
		assert_eq!(
			tail[0].event,
			RuntimeEvent::Recovery(Event::<T>::AttemptInitiated {
				lost: ALICE.into(),
				friend_group_index: 0,
				initiator: BOB.into(),
			}),
		);
		assert_eq!(
			tail[1].event,
			RuntimeEvent::Recovery(Event::<T>::AttemptApproved {
				lost: ALICE.into(),
				friend_group_index: 0,
				friend: BOB.into(),
			}),
		);

		// The bitfield reflects Bob's auto-approval (Bob is index 0).
		assert_eq!(
			Recovery::attempts(ALICE),
			vec![(
				fg([BOB, CHARLIE, DAVE]),
				AttemptOf::<T> {
					friend_group_index: 0,
					initiator: BOB.into(),
					init_block: 1,
					last_approval_block: 1,
					approvals: ApprovalBitfield::default().with_bits([0]).unwrap(),
				}
			)]
		);
	});
}

/// Approving an attempt works as expected.
#[test]
fn approve_attempt_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		// Bob is auto-approved as the initiator (index 0).
		assert_eq!(
			Recovery::attempts(ALICE),
			vec![(
				fg([BOB, CHARLIE, DAVE]),
				AttemptOf::<T> {
					friend_group_index: 0,
					initiator: BOB.into(),
					init_block: 1,
					last_approval_block: 1,
					approvals: ApprovalBitfield::default().with_bits([0]).unwrap(),
				}
			)]
		);
		// Bob cannot approve again.
		assert_noop!(Recovery::approve_attempt(signed(BOB), ALICE, 0), Error::<T>::AlreadyVoted);

		// Dave votes at block 3
		frame_system::Pallet::<T>::set_block_number(3);
		assert_ok!(Recovery::approve_attempt(signed(DAVE), ALICE, 0));
		assert_noop!(
			Recovery::approve_attempt(signed(DAVE), ALICE, 0),
			Error::<T>::AlreadyApproved
		);
		assert_eq!(
			Recovery::attempts(ALICE),
			vec![(
				fg([BOB, CHARLIE, DAVE]),
				AttemptOf::<T> {
					friend_group_index: 0,
					initiator: BOB.into(),
					init_block: 1,
					last_approval_block: 3,
					approvals: ApprovalBitfield::default().with_bits([0, 2]).unwrap(), /* Bob is index 0,
					                                                                    * Dave is
					                                                                    * 2 */
				}
			)]
		);

		// Non-friend cannot vote
		assert_noop!(Recovery::approve_attempt(signed(EVE), ALICE, 0), Error::<T>::NotFriend);

		// Charlie voting will fail since it is already approved
		frame_system::Pallet::<T>::set_block_number(4);
		assert_noop!(
			Recovery::approve_attempt(signed(CHARLIE), ALICE, 0),
			Error::<T>::AlreadyApproved
		);

		// Charlie cannot finish before the inheritance delay
		assert_noop!(
			Recovery::finish_attempt(signed(CHARLIE), ALICE, 0),
			Error::<T>::NotYetInheritable
		);

		// .. but can exactly at the unlock block
		hypothetically!({
			frame_system::Pallet::<T>::set_block_number(11);
			assert_ok!(Recovery::finish_attempt(signed(CHARLIE), ALICE, 0));
			assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
			assert_eq!(Recovery::inheritance(FERDIE), vec![ALICE]);
		});
		// .. or later
		frame_system::Pallet::<T>::set_block_number(25);
		assert_ok!(Recovery::finish_attempt(signed(CHARLIE), ALICE, 0));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert_eq!(Recovery::inheritance(FERDIE), vec![ALICE]);
	});
}

/// Can inherit multiple accounts.
#[test]
fn inherit_multiple_accounts_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		// setup bob friend groups
		let fgs = vec![fg([ALICE, CHARLIE, DAVE])];
		assert_ok!(Recovery::set_friend_groups(signed(BOB), fgs));

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));
		frame_system::Pallet::<T>::set_block_number(11);
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 0));

		assert_ok!(Recovery::initiate_attempt(signed(ALICE), BOB, 0));
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), BOB, 0));
		frame_system::Pallet::<T>::set_block_number(21);
		assert_ok!(Recovery::finish_attempt(signed(ALICE), BOB, 0));

		// Ferdie inherits both
		assert_eq!(Recovery::inheritor(BOB), Some(FERDIE));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert_eq!(Recovery::inheritance(FERDIE), vec![ALICE, BOB]);
	});
}

/// Finish attempt works
#[test]
fn finish_attempt_works() {
	new_test_ext().execute_with(|| {
		assert_err!(Recovery::finish_attempt(signed(BOB), ALICE, 0), Error::<T>::NotAttempt);
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);
		assert_err!(Recovery::finish_attempt(signed(BOB), ALICE, 0), Error::<T>::NotAttempt);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		// Bob is auto-approved as initiator, but threshold (2) is not yet reached.
		assert_err!(Recovery::finish_attempt(signed(BOB), ALICE, 0), Error::<T>::NotApproved);

		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));
		assert_err!(Recovery::finish_attempt(signed(BOB), ALICE, 0), Error::<T>::NotYetInheritable);

		frame_system::Pallet::<T>::set_block_number(11);
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 0));
		assert_err!(Recovery::finish_attempt(signed(BOB), ALICE, 0), Error::<T>::NotAttempt);

		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert_eq!(Recovery::inheritance(FERDIE), vec![ALICE]);
	});
}

/// Finish attempt works at the exact inheritance_delay boundary.
#[test]
fn finish_attempt_at_exact_boundary_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));

		// inheritance_delay is 10, init at block 1, so exactly block 11 should work
		System::set_block_number(10);
		assert_noop!(
			Recovery::finish_attempt(signed(EVE), ALICE, 0),
			Error::<T>::NotYetInheritable
		);

		System::set_block_number(11);
		assert_ok!(Recovery::finish_attempt(signed(EVE), ALICE, 0));
	});
}

/// Higher priority overwrites lower priority inheritor.
#[test]
fn inheritance_priority_conflict_overwrite() {
	new_test_ext().execute_with(|| {
		// EVE is inheritor with priority 1
		let ticket = Recovery::inheritor_ticket(&EVE).unwrap();
		Inheritor::<T>::insert(ALICE, (1, &EVE, ticket));

		// Add friend group with priority 0
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));
		frame_system::Pallet::<T>::set_block_number(11);

		// Eve is still inheritor
		assert_eq!(Recovery::inheritor(ALICE), Some(EVE));
		assert!(can_control_account(EVE, ALICE));
		// Eve has the inheritor deposit
		assert_inheritor_deposit(EVE, 14);

		// But now Ferdie will kick Eve out
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 0));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert!(can_control_account(FERDIE, ALICE));
		// Bob has the inheritor deposit
		assert_inheritor_deposit(BOB, 14);
		// Eve was kicked out
		assert!(!can_control_account(EVE, ALICE));
		assert_inheritor_deposit(EVE, 0);
	});
}

/// Friend group with equal or lower priority gets prevented from initiating an attempt.
#[test]
fn lower_priority_gets_rejected() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		// A friend group with priority 0 got it
		let ticket = Recovery::inheritor_ticket(&FERDIE).unwrap();
		Inheritor::<T>::insert(ALICE, (0, &FERDIE, ticket));

		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert!(can_control_account(FERDIE, ALICE));

		// Group 1 cannot initiate an attempt
		assert_noop!(
			Recovery::initiate_attempt(signed(BOB), ALICE, 0),
			Error::<T>::HigherPriorityRecovered
		);
	});
}

/// Controlling inherited account works
#[test]
fn control_inherited_account_works() {
	new_test_ext().execute_with(|| {
		// Mark FERDIE as the inheritor of ALICE
		let ticket = Recovery::inheritor_ticket(&FERDIE).unwrap();
		Inheritor::<T>::insert(ALICE, (0, &FERDIE, ticket));

		let call: RuntimeCall =
			BalancesCall::transfer_allow_death { value: 2 * START_BALANCE, dest: FERDIE }.into();
		let call_hash = call.using_encoded(<T as frame_system::Config>::Hashing::hash);

		// Outer call works:
		assert_ok!(Recovery::control_inherited_account(signed(FERDIE), ALICE, Box::new(call)));
		// Inner call fails:
		assert_last_event(Event::<T>::RecoveredAccountControlled {
			recovered: ALICE,
			inheritor: FERDIE,
			call_hash,
			call_result: Err(DispatchError::Token(TokenError::FundsUnavailable)),
		});
	});
}

/// Non-inheritor cannot control inherited account.
#[test]
fn control_inherited_account_not_inheritor_fails() {
	new_test_ext().execute_with(|| {
		let ticket = Recovery::inheritor_ticket(&FERDIE).unwrap();
		Inheritor::<T>::insert(ALICE, (0, &FERDIE, ticket));

		let call: RuntimeCall = frame_system::Call::remark { remark: vec![] }.into();

		assert_noop!(
			Recovery::control_inherited_account(signed(CHARLIE), ALICE, Box::new(call)),
			Error::<T>::NotInheritor
		);
	});
}

/// Cannot control account that has no inheritor set.
#[test]
fn control_inherited_account_no_inheritor_fails() {
	new_test_ext().execute_with(|| {
		let call: RuntimeCall = frame_system::Call::remark { remark: vec![] }.into();

		assert_noop!(
			Recovery::control_inherited_account(signed(BOB), ALICE, Box::new(call)),
			Error::<T>::NoInheritor
		);
	});
}

#[test]
fn cancel_attempt_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		assert_attempt_deposit(BOB, 48);

		// Charlie can never cancel
		assert_noop!(Recovery::cancel_attempt(signed(CHARLIE), ALICE, 0), Error::<T>::NotCanceller);
		// Bob can not yet cancel
		assert_noop!(Recovery::cancel_attempt(signed(BOB), ALICE, 0), Error::<T>::NotYetCancelable);
		// Lost could always cancel
		hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(ALICE), ALICE, 0));
			assert_attempt_deposit(BOB, 0);
			assert_eq!(<Test as Config>::Currency::total_balance(&BOB), START_BALANCE);
		});

		frame_system::Pallet::<T>::set_block_number(12);
		// Charlie can never cancel
		assert_noop!(Recovery::cancel_attempt(signed(CHARLIE), ALICE, 0), Error::<T>::NotCanceller);
		// Bob could cancel
		hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(BOB), ALICE, 0));
			assert_last_event(Event::<T>::AttemptCanceled {
				lost: ALICE,
				friend_group_index: 0,
				canceler: BOB,
			});
			assert_attempt_deposit(BOB, 0);
			assert_eq!(<Test as Config>::Currency::total_balance(&BOB), START_BALANCE);
		});
		// Alice could cancel
		hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(ALICE), ALICE, 0));
			assert_last_event(Event::<T>::AttemptCanceled {
				lost: ALICE,
				friend_group_index: 0,
				canceler: ALICE,
			});
			assert_attempt_deposit(BOB, 0);
			assert_eq!(<Test as Config>::Currency::total_balance(&BOB), START_BALANCE);
		});

		// Alice or Bob canceling is the exact same thing
		let alice_cancel = hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(ALICE), ALICE, 0));
			root_without_events()
		});
		let bob_cancel = hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(BOB), ALICE, 0));
			root_without_events()
		});
		assert_eq!(alice_cancel, bob_cancel);
	});
}

#[test]
fn cancel_attempt_extends_delay_after_new_approval() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		// Let's go just before the DELAY expires
		inc_block_number(ABORT_DELAY - 1);
		// After one more block, Bob can cancel
		hypothetically!({
			inc_block_number(1);
			assert_ok!(Recovery::cancel_attempt(signed(BOB), ALICE, 0));
		});

		// But if there is a new approval, the delay is extended
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));
		hypothetically!({
			inc_block_number(1);
			assert_noop!(
				Recovery::cancel_attempt(signed(BOB), ALICE, 0),
				Error::<T>::NotYetCancelable
			);
		});

		// Bob needs to wait for the DELAY again
		inc_block_number(ABORT_DELAY);
		assert_ok!(Recovery::cancel_attempt(signed(BOB), ALICE, 0));
	});
}

/// Can still cancel an attempt even if the initiator account does not have the deposit anymore.
#[test]
fn cancel_attempt_works_when_initiator_account_is_broken() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		assert_attempt_deposit(BOB, 48);

		frame_system::Pallet::<T>::set_block_number(12);

		// Force remove the deposit from bob
		assert_ok!(<T as Config>::Currency::set_balance_on_hold(
			&crate::HoldReason::AttemptStorage.into(),
			&BOB,
			0
		));
		assert_attempt_deposit(BOB, 0);

		// Bob could cancel
		hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(BOB), ALICE, 0));
			assert_last_event(Event::<T>::AttemptCanceled {
				lost: ALICE,
				friend_group_index: 0,
				canceler: BOB,
			});
		});
		// Alice could cancel
		hypothetically!({
			assert_ok!(Recovery::cancel_attempt(signed(ALICE), ALICE, 0));
			assert_last_event(Event::<T>::AttemptCanceled {
				lost: ALICE,
				friend_group_index: 0,
				canceler: ALICE,
			});
		});
	});
}

/// Cancel attempt works at the exact cancel_delay boundary.
#[test]
fn cancel_attempt_at_exact_boundary_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		// cancel_delay is ABORT_DELAY (5), last_approval at block 1
		System::set_block_number(5);
		assert_noop!(Recovery::cancel_attempt(signed(BOB), ALICE, 0), Error::<T>::NotYetCancelable);

		System::set_block_number(6);
		assert_ok!(Recovery::cancel_attempt(signed(BOB), ALICE, 0));
	});
}

/// Slashing an attempt will remove it and slash the deposit of the initiator.
#[test]
fn slash_attempt_works() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		assert_security_deposit(BOB, SECURITY_DEPOSIT);

		// Slash the attempt and check that TI decreased
		let ti = <T as Config>::Currency::total_issuance();
		assert_ok!(Recovery::slash_attempt(signed(ALICE), 0));
		assert_security_deposit(BOB, 0);

		// Bob got slashed
		assert_eq!(<T as Config>::Currency::total_balance(&BOB), START_BALANCE - SECURITY_DEPOSIT);
		// TI reduced (balance burnt)
		assert_eq!(<T as Config>::Currency::total_issuance(), ti - SECURITY_DEPOSIT);

		assert_last_event(Event::<T>::AttemptSlashed { lost: ALICE, friend_group_index: 0 });
	});
}

#[test]
fn slash_attempt_fails_when_initiator_is_missing_deposit() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		assert_attempt_deposit(BOB, 48);

		// Force remove the deposit from bob
		assert_ok!(<T as Config>::Currency::set_balance_on_hold(
			&crate::HoldReason::AttemptStorage.into(),
			&BOB,
			0
		));
		assert_attempt_deposit(BOB, 0);

		// Slash the attempt
		assert_ok!(Recovery::slash_attempt(signed(ALICE), 0));
	});
}

/// Bitfield tests
#[test]
fn bitfield_default_all_zeros() {
	// Test with 16 max entries (fits in 1 u16 word)
	let bitfield: Bitfield<ConstU32<16>> = Bitfield::default();
	assert_eq!(bitfield.count_ones(), 0);
	assert_eq!(bitfield.0.len(), 1);

	// Test with 32 max entries (fits in 2 u16 words)
	let bitfield: Bitfield<ConstU32<32>> = Bitfield::default();
	assert_eq!(bitfield.count_ones(), 0);
	assert_eq!(bitfield.0.len(), 2);

	// Test with 17 max entries (needs 2 u16 words due to ceiling division)
	let bitfield: Bitfield<ConstU32<17>> = Bitfield::default();
	assert_eq!(bitfield.count_ones(), 0);
	assert_eq!(bitfield.0.len(), 2);
}

#[test]
fn bitfield_set_if_not_set_works() {
	let mut bitfield: Bitfield<ConstU32<32>> = Bitfield::default();

	// Set bit at index 0
	assert_ok!(bitfield.set_if_not_set(0));
	assert_eq!(bitfield.count_ones(), 1);

	// Try to set the same bit again - should fail
	assert_err!(bitfield.set_if_not_set(0), ());
	assert_eq!(bitfield.count_ones(), 1);

	// Set bit at index 15 (last bit of first u16 word)
	assert_ok!(bitfield.set_if_not_set(15));
	assert_eq!(bitfield.count_ones(), 2);

	// Set bit at index 16 (first bit of second u16 word)
	assert_ok!(bitfield.set_if_not_set(16));
	assert_eq!(bitfield.count_ones(), 3);

	// Set bit at index 31 (last bit of second u16 word)
	assert_ok!(bitfield.set_if_not_set(31));
	assert_eq!(bitfield.count_ones(), 4);
}

#[test]
fn bitfield_set_out_of_bounds_fails() {
	let mut bitfield: Bitfield<ConstU32<16>> = Bitfield::default();

	// Valid indices 0-15
	assert_ok!(bitfield.set_if_not_set(0));
	assert_ok!(bitfield.set_if_not_set(15));

	// Index 16 is out of bounds for MaxEntries=16
	assert_err!(bitfield.set_if_not_set(16), ());
	assert_err!(bitfield.set_if_not_set(100), ());
}

#[test]
fn bitfield_count_ones_works() {
	let mut bitfield: Bitfield<ConstU32<64>> = Bitfield::default();
	assert_eq!(bitfield.count_ones(), 0);

	// Set bits across multiple u16 words
	for i in [0, 5, 10, 15, 16, 20, 31, 32, 48, 63] {
		assert_ok!(bitfield.set_if_not_set(i));
	}

	assert_eq!(bitfield.count_ones(), 10);
}

#[test]
fn bitfield_with_bits_helper_works() {
	let bitfield: Bitfield<ConstU32<32>> =
		Bitfield::default().with_bits([0, 5, 10, 15, 20, 25]).unwrap();
	assert_eq!(bitfield.count_ones(), 6);

	// Verify individual bits are set
	let mut test_bitfield = Bitfield::default();
	assert_ok!(test_bitfield.set_if_not_set(0));
	assert_ok!(test_bitfield.set_if_not_set(5));
	assert_ok!(test_bitfield.set_if_not_set(10));
	assert_ok!(test_bitfield.set_if_not_set(15));
	assert_ok!(test_bitfield.set_if_not_set(20));
	assert_ok!(test_bitfield.set_if_not_set(25));

	assert_eq!(bitfield, test_bitfield);
}

#[test]
fn bitfield_with_bits_rejects_duplicate_index() {
	let result = Bitfield::<ConstU32<32>>::default().with_bits([3, 3]);
	assert_eq!(result, Err(()));
}

#[test]
fn bitfield_with_bits_rejects_out_of_bounds() {
	// Capacity is 32 bits → index 32 is out of bounds.
	let result = Bitfield::<ConstU32<32>>::default().with_bits([32]);
	assert_eq!(result, Err(()));
}

#[test]
fn bitfield_multiple_words_works() {
	// Test with exactly 48 entries (3 u16 words)
	let mut bitfield: Bitfield<ConstU32<48>> = Bitfield::default();
	assert_eq!(bitfield.0.len(), 3);

	// Set one bit in each word
	assert_ok!(bitfield.set_if_not_set(0)); // First word
	assert_ok!(bitfield.set_if_not_set(16)); // Second word
	assert_ok!(bitfield.set_if_not_set(32)); // Third word
	assert_eq!(bitfield.count_ones(), 3);

	// Fill the first word completely (bits 0-15)
	for i in 0..16 {
		let _ = bitfield.set_if_not_set(i);
	}
	assert_eq!(bitfield.count_ones(), 16 + 1 + 1); // 16 in first word + 1 in second + 1 in third
}

#[test]
fn bitfield_already_voted_scenario() {
	// Simulates the approval scenario from the recovery pallet
	let mut bitfield: Bitfield<ConstU32<10>> = Bitfield::default();

	// Friend at index 0 votes
	assert_ok!(bitfield.set_if_not_set(0));

	// Friend at index 0 tries to vote again - should fail
	assert_err!(bitfield.set_if_not_set(0), ());

	// Friend at index 5 votes
	assert_ok!(bitfield.set_if_not_set(5));

	// Friend at index 9 votes
	assert_ok!(bitfield.set_if_not_set(9));

	assert_eq!(bitfield.count_ones(), 3);
}

#[test]
fn bitfield_ceiling_division_works() {
	// Test that ceiling division works correctly for non-multiples of 16

	// 1 entry needs 1 word
	let bitfield: Bitfield<ConstU32<1>> = Bitfield::default();
	assert_eq!(bitfield.0.len(), 1);

	// 15 entries needs 1 word
	let bitfield: Bitfield<ConstU32<15>> = Bitfield::default();
	assert_eq!(bitfield.0.len(), 1);

	// 16 entries needs 1 word
	let bitfield: Bitfield<ConstU32<16>> = Bitfield::default();
	assert_eq!(bitfield.0.len(), 1);

	// 17 entries needs 2 words (ceiling division)
	let bitfield: Bitfield<ConstU32<17>> = Bitfield::default();
	assert_eq!(bitfield.0.len(), 2);

	// 33 entries needs 3 words (ceiling division)
	let bitfield: Bitfield<ConstU32<33>> = Bitfield::default();
	assert_eq!(bitfield.0.len(), 3);
}

#[test]
fn bitfield_all_bits_in_word_set() {
	let mut bitfield: Bitfield<ConstU32<16>> = Bitfield::default();

	// Set all 16 bits
	for i in 0..16 {
		assert_ok!(bitfield.set_if_not_set(i));
	}

	assert_eq!(bitfield.count_ones(), 16);

	// All bits should now fail to set again
	for i in 0..16 {
		assert_err!(bitfield.set_if_not_set(i), ());
	}
}

#[test]
fn slash_handles_different_receivers() {
	new_test_ext().execute_with(|| {
		setup_alice_fgs([[BOB, CHARLIE, DAVE]]);
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));

		let ti_before = <T as Config>::Currency::total_issuance();
		let bob_balance_before = <T as Config>::Currency::total_balance(&BOB);

		// Slash burns when no receiver
		hypothetically!({
			SlashReceiverAccount::set(&None);
			assert_ok!(Recovery::slash_attempt(signed(ALICE), 0));

			assert_eq!(
				<T as Config>::Currency::total_balance(&BOB),
				bob_balance_before - SECURITY_DEPOSIT
			);
			assert_eq!(<T as Config>::Currency::total_issuance(), ti_before - SECURITY_DEPOSIT);
		});

		// Slash transfers to existing receiver
		hypothetically!({
			SlashReceiverAccount::set(&Some(EVE));
			let eve_balance_before = <T as Config>::Currency::total_balance(&EVE);

			assert_ok!(Recovery::slash_attempt(signed(ALICE), 0));

			assert_eq!(
				<T as Config>::Currency::total_balance(&BOB),
				bob_balance_before - SECURITY_DEPOSIT
			);
			assert_eq!(
				<T as Config>::Currency::total_balance(&EVE),
				eve_balance_before + SECURITY_DEPOSIT
			);
			assert_eq!(<T as Config>::Currency::total_issuance(), ti_before);
		});

		// Slash transfers to non-existing receiver, creating it
		hypothetically!({
			const TREASURY: u64 = 999;
			SlashReceiverAccount::set(&Some(TREASURY));
			assert_eq!(<T as Config>::Currency::total_balance(&TREASURY), 0);

			assert_ok!(Recovery::slash_attempt(signed(ALICE), 0));

			assert_eq!(
				<T as Config>::Currency::total_balance(&BOB),
				bob_balance_before - SECURITY_DEPOSIT
			);
			assert_eq!(<T as Config>::Currency::total_balance(&TREASURY), SECURITY_DEPOSIT);
			assert_eq!(<T as Config>::Currency::total_issuance(), ti_before);
		});
	});
}

/// Setting more than MAX_GROUPS_PER_ACCOUNT friend groups fails.
#[test]
fn set_friend_groups_too_many_groups_fails() {
	new_test_ext().execute_with(|| {
		let friend_groups: Vec<_> = (0..=MAX_GROUPS_PER_ACCOUNT as u8)
			.map(|i| FriendGroupOf::<T> {
				friends: friends([BOB, 10 + i as u64]),
				friends_needed: 1,
				inheritor: FERDIE,
				inheritance_delay: 10,
				inheritance_priority: i as u32,
				cancel_delay: 10,
			})
			.collect();

		assert_noop!(
			Recovery::set_friend_groups(signed(ALICE), friend_groups),
			Error::<T>::TooManyFriendGroups
		);
	});
}

/// Regression: `IdentifiedConsideration::update` must not leak the old deposit when the depositor
/// is unchanged.
#[test]
fn finish_attempt_same_caller_does_not_leak_inheritor_deposit() {
	new_test_ext().execute_with(|| {
		let fg0 = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 1,
			cancel_delay: 10,
		};
		let fg1 = FriendGroupOf::<T> {
			friends: friends([BOB, EVE]),
			friends_needed: 2,
			inheritor: DAVE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg0, fg1]));

		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 1));

		// BOB is auto-approved as initiator in both groups; one extra vote each suffices.
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));
		assert_ok!(Recovery::approve_attempt(signed(EVE), ALICE, 1));

		frame_system::Pallet::<T>::set_block_number(11);

		// BOB finishes group 0 (priority 1)
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 0));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert_inheritor_deposit(BOB, 14);

		// BOB finishes group 1 (priority 0) — replaces inheritor, same depositor
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 1));
		assert_eq!(Recovery::inheritor(ALICE), Some(DAVE));
		assert_inheritor_deposit(BOB, 14);
	});
}

/// Revoking the inheritor works when called by the lost account.
#[test]
fn revoke_inheritor_works() {
	new_test_ext().execute_with(|| {
		// Set up an inheritor for ALICE
		let ticket = Recovery::inheritor_ticket(&EVE).unwrap();
		Inheritor::<T>::insert(ALICE, (0, &FERDIE, ticket));

		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert_inheritor_deposit(EVE, 14);

		// Alice revokes the inheritor
		assert_ok!(Recovery::revoke_inheritor(signed(ALICE)));

		// Storage cleared
		assert_eq!(Recovery::inheritor(ALICE), None);
		// Deposit refunded
		assert_inheritor_deposit(EVE, 0);
		// Event emitted
		assert_last_event(Event::<T>::InheritorRevoked { lost: ALICE });
	});
}

/// Revoking when no inheritor exists fails.
#[test]
fn revoke_inheritor_no_inheritor_fails() {
	new_test_ext().execute_with(|| {
		assert_noop!(Recovery::revoke_inheritor(signed(ALICE)), Error::<T>::NoInheritor);
	});
}

/// Non-lost account cannot revoke someone else's inheritor.
#[test]
fn revoke_inheritor_not_lost_account_fails() {
	new_test_ext().execute_with(|| {
		let ticket = Recovery::inheritor_ticket(&EVE).unwrap();
		Inheritor::<T>::insert(ALICE, (0, &FERDIE, ticket));

		// BOB tries to revoke ALICE's inheritor - no such extrinsic parameter, so
		// BOB calling revoke_inheritor only affects BOB's own entry (which doesn't exist).
		assert_noop!(Recovery::revoke_inheritor(signed(BOB)), Error::<T>::NoInheritor);

		// ALICE's inheritor is unchanged
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
	});
}

/// Finishing an attempt when an equal-or-higher priority inheritor already exists does not
/// replace it.
///
/// Both groups initiate before any inheritor exists. The higher-priority group finishes first,
/// then the lower-priority group finishes but does not replace the inheritor.
#[test]
fn finish_attempt_lower_priority_does_not_replace() {
	new_test_ext().execute_with(|| {
		// Two friend groups: group 0 (priority 0) and group 1 (priority 1)
		let fg0 = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE, DAVE]),
			friends_needed: 2,
			inheritor: FERDIE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 10,
		};
		let fg1 = FriendGroupOf::<T> {
			friends: friends([BOB, EVE]),
			friends_needed: 1,
			inheritor: DAVE,
			inheritance_delay: 10,
			inheritance_priority: 1,
			cancel_delay: 10,
		};
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![fg0, fg1]));

		// Both groups initiate while no inheritor exists
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 1));

		// Group 1 (priority 1, friends_needed=1) is fully approved by BOB's auto-approval.
		// Group 0 (priority 0) needs CHARLIE on top of BOB's auto-approval.
		assert_ok!(Recovery::approve_attempt(signed(CHARLIE), ALICE, 0));

		frame_system::Pallet::<T>::set_block_number(11);

		// Group 0 finishes first — FERDIE becomes inheritor
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 0));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));

		// Group 1 finishes — DAVE does NOT replace FERDIE since priority 1 < priority 0
		assert_ok!(Recovery::finish_attempt(signed(BOB), ALICE, 1));
		assert_last_event(Event::<T>::AttemptDiscarded {
			lost: ALICE,
			friend_group_index: 1,
			existing_inheritor: FERDIE,
		});
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));
		assert!(can_control_account(FERDIE, ALICE));
		assert!(!can_control_account(DAVE, ALICE));
	});
}

/// Regression test to ensure a malicious controller cannot dismiss higher-priority attempts.
#[test]
fn inheritor_can_slash_higher_priority_attempts_and_remove_friend_groups() {
	new_test_ext().execute_with(|| {
		// Alice configures two friend groups:
		//   Group 0 (priority 0, "Family"): BOB, CHARLIE — higher priority
		//   Group 1 (priority 1, "Friends"): DAVE, EVE — lower priority, inheritor = FERDIE
		let family = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE]),
			friends_needed: 1,
			inheritor: DAVE, // Family's chosen inheritor
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 5,
		};
		let friends_group = FriendGroupOf::<T> {
			friends: friends([CHARLIE, EVE]),
			friends_needed: 1,
			inheritor: FERDIE, // Friends' chosen inheritor
			inheritance_delay: 1,
			inheritance_priority: 1,
			cancel_delay: 5,
		};
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![family, friends_group]));

		// --- Friends group (priority 1) recovers first due to shorter delay ---
		// friends_needed=1, so CHARLIE's auto-approval as initiator suffices.
		assert_ok!(Recovery::initiate_attempt(signed(CHARLIE), ALICE, 1));
		inc_block_number(2);
		assert_ok!(Recovery::finish_attempt(signed(EVE), ALICE, 1));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));

		// --- Family group (priority 0) initiates a higher-priority attempt ---
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		let bob_balance_before = <Test as Config>::Currency::total_balance(&BOB);

		// Attack vector A: FERDIE tries to slash Family's attempt via the proxy.
		assert_ok!(Recovery::control_inherited_account(
			signed(FERDIE),
			ALICE,
			Box::new(RecoveryCall::slash_attempt { friend_group_index: 0 }.into())
		));

		// BOB's security deposit must NOT have been slashed
		let bob_balance_after = <Test as Config>::Currency::total_balance(&BOB);
		if bob_balance_after < bob_balance_before {
			panic!(
				"VULNERABILITY: inheritor slashed a higher-priority attempt via proxy! \
				 BOB lost {} from security deposit.",
				bob_balance_before - bob_balance_after
			);
		}
		// The family attempt must still be alive
		assert!(!Recovery::attempts(ALICE).is_empty(), "Family attempt was destroyed");

		// Attack vector B: FERDIE tries to remove all friend groups via the proxy.
		assert_ok!(Recovery::control_inherited_account(
			signed(FERDIE),
			ALICE,
			Box::new(RecoveryCall::set_friend_groups { friend_groups: vec![] }.into())
		));

		// Friend groups must still be intact
		assert!(
			!Recovery::friend_groups(ALICE).is_empty(),
			"VULNERABILITY: inheritor removed all friend groups via proxy"
		);
	});
}

/// Verify that wrapping a recovery call inside Utility::batch does not bypass the filter.
#[test]
fn inheritor_cannot_bypass_filter_via_utility_batch() {
	new_test_ext().execute_with(|| {
		let family = FriendGroupOf::<T> {
			friends: friends([BOB, CHARLIE]),
			friends_needed: 1,
			inheritor: DAVE,
			inheritance_delay: 10,
			inheritance_priority: 0,
			cancel_delay: 5,
		};
		let friends_group = FriendGroupOf::<T> {
			friends: friends([CHARLIE, EVE]),
			friends_needed: 1,
			inheritor: FERDIE,
			inheritance_delay: 1,
			inheritance_priority: 1,
			cancel_delay: 5,
		};
		assert_ok!(Recovery::set_friend_groups(signed(ALICE), vec![family, friends_group]));

		// Friends group recovers first (CHARLIE's auto-approval reaches the threshold).
		assert_ok!(Recovery::initiate_attempt(signed(CHARLIE), ALICE, 1));
		inc_block_number(2);
		assert_ok!(Recovery::finish_attempt(signed(EVE), ALICE, 1));
		assert_eq!(Recovery::inheritor(ALICE), Some(FERDIE));

		// Family initiates higher-priority attempt
		assert_ok!(Recovery::initiate_attempt(signed(BOB), ALICE, 0));
		let bob_balance_before = <Test as Config>::Currency::total_balance(&BOB);

		// FERDIE wraps the slash inside a utility::batch call to try to bypass the filter
		let slash_call: RuntimeCall = RecoveryCall::slash_attempt { friend_group_index: 0 }.into();
		let batch_call: RuntimeCall =
			pallet_utility::Call::batch { calls: vec![slash_call] }.into();
		assert_ok!(Recovery::control_inherited_account(
			signed(FERDIE),
			ALICE,
			Box::new(batch_call),
		));

		// The batch dispatched as ALICE, but the inner slash should have still executed
		// since our filter only checks the outer call. Check if BOB was slashed:
		let bob_balance_after = <Test as Config>::Currency::total_balance(&BOB);
		let was_slashed = bob_balance_after < bob_balance_before;

		if was_slashed {
			panic!(
				"BYPASS: recovery call filter was circumvented via utility::batch! \
				 BOB lost {} from security deposit slash.",
				bob_balance_before - bob_balance_after
			);
		}
	});
}
