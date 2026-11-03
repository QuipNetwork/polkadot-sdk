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

//! Bounties pallet tests.

#![cfg(test)]

use super::{Event as BountiesEvent, *};
use crate as pallet_bounties;
use crate::mock::{Bounties, *};

use frame_support::{
	assert_err_ignore_postinfo, assert_noop, assert_ok,
	traits::{fungible::Mutate, Currency},
};
use sp_runtime::{traits::Dispatchable, TokenError};

type UtilityCall = pallet_utility::Call<Test>;
type BountiesCall = crate::Call<Test>;

#[docify::export]
#[test]
fn fund_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let asset_kind = 1;
		let value = 50;
		let curator = 4;
		let metadata = note_preimage(1);
		let _ = Balances::mint_into(&curator, Balances::minimum_balance());

		// When
		assert_ok!(Bounties::fund_bounty(
			RuntimeOrigin::root(),
			Box::new(asset_kind),
			value,
			curator,
			metadata
		));

		// Then
		let parent_bounty_id = 0;
		let bounty_account =
			Bounties::bounty_account(parent_bounty_id, asset_kind).expect("conversion failed");
		let payment_id = get_payment_id(parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(paid(bounty_account, asset_kind), value);
		expect_events(vec![
			BountiesEvent::Paid { index: parent_bounty_id, child_index: None, payment_id },
			BountiesEvent::BountyCreated { index: parent_bounty_id },
		]);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(parent_bounty_id).unwrap(),
			Bounty {
				asset_kind,
				value,
				metadata,
				status: BountyStatus::FundingAttempted {
					curator,
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(pallet_bounties::BountyCount::<Test>::get(), 1);
		assert!(Preimage::is_requested(&metadata));
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(parent_bounty_id, None::<BountyIndex>),
			None
		);
	});
}

#[test]
fn fund_bounty_in_batch_respects_max_total() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let asset_kind = 1;
		let spend_origin = 10; // max spending of 10
		let value = 2; // `native_amount` is 2
		let curator = 4;
		let metadata = note_preimage(1);
		let _ = Balances::mint_into(&curator, Balances::minimum_balance());

		// When/Then
		// Respect the `max_total` for the given origin.
		assert_ok!(RuntimeCall::from(UtilityCall::batch_all {
			calls: vec![
				RuntimeCall::from(BountiesCall::fund_bounty {
					asset_kind: Box::new(asset_kind),
					value,
					curator,
					metadata
				}),
				RuntimeCall::from(BountiesCall::fund_bounty {
					asset_kind: Box::new(asset_kind),
					value,
					curator,
					metadata
				})
			]
		})
		.dispatch(RuntimeOrigin::signed(spend_origin)));

		// Given
		let value = 5; // `native_amount` is 5

		// When/Then
		// `spend` of 10 surpasses `max_total` for the given origin.
		assert_err_ignore_postinfo!(
			RuntimeCall::from(UtilityCall::batch_all {
				calls: vec![
					RuntimeCall::from(BountiesCall::fund_bounty {
						asset_kind: Box::new(asset_kind),
						value,
						curator,
						metadata
					}),
					RuntimeCall::from(BountiesCall::fund_bounty {
						asset_kind: Box::new(asset_kind),
						value,
						curator,
						metadata
					})
				]
			})
			.dispatch(RuntimeOrigin::signed(spend_origin)),
			Error::<Test>::InsufficientPermission
		);
	});
}

#[test]
fn fund_bounty_second_instance_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let asset_kind = 1;
		let value = 10;
		let curator = 4;
		let parent_bounty_id = 0;
		let parent_bounty_account_1 =
			Bounties::bounty_account(parent_bounty_id, asset_kind).expect("conversion failed");
		let parent_bounty_account_2 =
			Bounties1::bounty_account(parent_bounty_id, asset_kind).expect("conversion failed");
		let metadata = note_preimage(1);
		let _ = Balances::mint_into(&curator, Balances::minimum_balance());

		// When
		assert_ok!(Bounties1::fund_bounty(
			RuntimeOrigin::root(),
			Box::new(asset_kind),
			value,
			curator,
			metadata
		));

		// Then
		assert_eq!(paid(parent_bounty_account_2, asset_kind), value); // Bounties 2 is funded
		assert_eq!(paid(parent_bounty_account_1, asset_kind), 0); // Bounties 1 is unchanged
	});
}

#[test]
fn fund_bounty_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let asset_kind = 1;
		let value = 50;
		let curator = 4;
		let metadata = note_preimage(1);
		let _ = Balances::mint_into(&curator, Balances::minimum_balance());

		// When/Then
		let invalid_origin = RuntimeOrigin::none();
		assert_noop!(
			Bounties::fund_bounty(invalid_origin, Box::new(asset_kind), value, curator, metadata),
			BadOrigin
		);

		// When/Then
		let invalid_origin = RuntimeOrigin::signed(0);
		assert_noop!(
			Bounties::fund_bounty(invalid_origin, Box::new(asset_kind), value, curator, metadata),
			BadOrigin
		);

		// When/Then
		let invalid_value = 1;
		assert_noop!(
			Bounties::fund_bounty(
				RuntimeOrigin::root(),
				Box::new(asset_kind),
				invalid_value,
				curator,
				metadata
			),
			Error::<Test>::InvalidValue
		);

		// When/Then
		let invalid_value = 11;
		assert_noop!(
			Bounties::fund_bounty(
				RuntimeOrigin::signed(10), // max spending of 10
				Box::new(asset_kind),
				invalid_value,
				curator,
				metadata
			),
			Error::<Test>::InsufficientPermission
		);

		// When/Then
		let invalid_metadata: <Test as frame_system::Config>::Hash = [1u8; 32].into();
		assert_noop!(
			Bounties::fund_bounty(
				RuntimeOrigin::root(),
				Box::new(asset_kind),
				value,
				curator,
				invalid_metadata
			),
			Error::<Test>::PreimageNotExist
		);

		// When/Then
		SpendLimit::set(50);
		let invalid_value = 51;
		assert_noop!(
			Bounties::fund_bounty(
				RuntimeOrigin::root(),
				Box::new(asset_kind),
				invalid_value,
				curator,
				metadata
			),
			Error::<Test>::InsufficientPermission
		);
	});
}

#[test]
fn fund_child_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let mut s = create_active_parent_bounty();

		// When
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			Some(s.child_curator),
		));
		s.child_bounty_id =
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id) - 1;

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, s.child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		assert_eq!(paid(child_bounty_account, s.asset_kind), s.child_value);
		expect_events(vec![
			BountiesEvent::Paid {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id,
			},
			BountiesEvent::ChildBountyCreated {
				index: s.parent_bounty_id,
				child_index: s.child_bounty_id,
			},
		]);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::FundingAttempted {
					curator: s.child_curator,
					payment_status: PaymentState::Attempted { id: payment_id }
				}
			}
		);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			1
		);
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 1);
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			s.child_value
		);
		assert!(Preimage::is_requested(&s.metadata));
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);

		// When
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			None,
		));
		s.child_bounty_id =
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id) - 1;

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		expect_events(vec![
			BountiesEvent::Paid {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id,
			},
			BountiesEvent::ChildBountyCreated {
				index: s.parent_bounty_id,
				child_index: s.child_bounty_id,
			},
		]);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::FundingAttempted {
					curator: s.curator,
					payment_status: PaymentState::Attempted { id: payment_id }
				}
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(s.value)
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);
	})
}

#[test]
fn fund_child_bounty_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_active_parent_bounty();

		// When/Then
		let invalid_origin = RuntimeOrigin::none();
		assert_noop!(
			Bounties::fund_child_bounty(
				invalid_origin,
				s.parent_bounty_id,
				s.child_value,
				s.metadata,
				Some(s.child_curator),
			),
			BadOrigin
		);

		// When/Then
		let invalid_parent_index = 2;
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				invalid_parent_index,
				s.child_value,
				s.metadata,
				None,
			),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		let invalid_value = 0;
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				invalid_value,
				s.metadata,
				None,
			),
			Error::<Test>::InvalidValue
		);

		// When/Then
		let invalid_metadata: <Test as frame_system::Config>::Hash = [1u8; 32].into();
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				s.child_value,
				invalid_metadata,
				None,
			),
			Error::<Test>::PreimageNotExist
		);

		// When/Then
		let invalid_origin = RuntimeOrigin::signed(1);
		assert_noop!(
			Bounties::fund_child_bounty(
				invalid_origin,
				s.parent_bounty_id,
				s.child_value,
				s.metadata,
				None,
			),
			Error::<Test>::RequireCurator
		);

		// When/Then
		let invalid_value = s.value + 1;
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				invalid_value,
				s.metadata,
				None,
			),
			Error::<Test>::InsufficientBountyValue
		);

		// Given
		MaxActiveChildBountyCount::set(1);
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			None,
		));

		// When/Then
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				s.child_value,
				s.metadata,
				None,
			),
			Error::<Test>::TooManyChildBounties
		);

		// Given
		let s = create_awarded_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				s.child_value,
				s.metadata,
				None,
			),
			Error::<Test>::UnexpectedStatus
		);
	})
}

#[test]
fn check_status_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status is `FundingAttempted` and payment fails
		let s = create_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Failure);

		// When
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::PaymentFailed {
				index: s.parent_bounty_id,
				child_index: None,
				payment_id
			}
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::FundingAttempted {
					curator: s.curator,
					payment_status: PaymentState::Failed
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);

		// Given: parent bounty status is `FundingAttempted` and payment succeeds
		let s = create_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::BountyFundingProcessed { index: s.parent_bounty_id, child_index: None }
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::Funded { curator: s.curator }
		);

		// Given: parent bounty status is `RefundAttempted` and payment fails
		let s = create_canceled_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Failure);

		// When
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::PaymentFailed {
				index: s.parent_bounty_id,
				child_index: None,
				payment_id
			}
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::RefundAttempted {
				curator: Some(s.curator),
				payment_status: PaymentState::Failed
			}
		);
		assert!(Preimage::is_requested(&s.metadata));
		assert_eq!(Balances::free_balance(s.curator), Balances::minimum_balance());
		assert_eq!(Balances::reserved_balance(s.curator), s.curator_deposit);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(s.value)
		);

		// Given: parent bounty status is `RefundAttempted` and payment succeeds
		let s = create_canceled_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::BountyRefundProcessed { index: s.parent_bounty_id, child_index: None }
		);
		assert_eq!(pallet_bounties::Bounties::<Test>::iter().count(), 4 - 1);
		assert_eq!(pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id), None);
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 0);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			0
		);
		assert!(!Preimage::is_requested(&s.metadata));
		assert_eq!(
			Balances::free_balance(s.curator),
			Balances::minimum_balance() + s.curator_deposit
		); // initial
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);

		// Given: parent bounty status is `PayoutAttempted` and payment fails
		let s = create_awarded_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Failure);

		// When
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		expect_events(vec![BountiesEvent::PaymentFailed {
			index: s.parent_bounty_id,
			child_index: None,
			payment_id,
		}]);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::PayoutAttempted {
				curator: s.curator,
				beneficiary: s.beneficiary,
				payment_status: PaymentState::Failed
			}
		);
		assert!(Preimage::is_requested(&s.metadata));
		assert_eq!(Balances::free_balance(s.curator), Balances::minimum_balance());
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(s.value)
		);

		// Given: parent bounty status is `PayoutAttempted` and payment succeeds
		let s = create_awarded_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		expect_events(vec![BountiesEvent::BountyPayoutProcessed {
			index: s.parent_bounty_id,
			child_index: None,
			asset_kind: s.asset_kind,
			value: s.value,
			beneficiary: s.beneficiary,
		}]);
		assert_eq!(pallet_bounties::Bounties::<Test>::iter().count(), 6 - 1 - 1);
		assert_eq!(pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id), None);
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 0);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			0
		);
		assert!(!Preimage::is_requested(&s.metadata));
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);
		assert_eq!(
			Balances::free_balance(s.curator),
			Balances::minimum_balance() + s.curator_deposit
		); // initial

		// Given: child-bounty status is `FundingAttempted` and payment fails
		let s = create_child_bounty_with_curator();
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Failure);

		// When
		assert_ok!(Bounties::check_status(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::PaymentFailed {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::FundingAttempted {
					curator: s.child_curator,
					payment_status: PaymentState::Failed
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);

		// Given: child-bounty with curator status is `FundingAttempted` and payment succeeds
		let s = create_child_bounty_with_curator();
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When
		assert_ok!(Bounties::check_status(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::BountyFundingProcessed {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::Funded { curator: s.child_curator },
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::iter_prefix(s.parent_bounty_id).count(),
			1
		);

		// Given: child-bounty with curator and status `RefundAttempted` and payment fails
		let s = create_canceled_child_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Failure);

		// When
		assert_ok!(Bounties::check_status(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::PaymentFailed {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::RefundAttempted {
				curator: Some(s.child_curator),
				payment_status: PaymentState::Failed
			},
		);
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			s.child_value
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::iter_prefix(s.parent_bounty_id).count(),
			1
		);
		assert!(Preimage::is_requested(&s.metadata));
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			)
			.unwrap(),
			consideration(s.child_value)
		);
		assert_eq!(Balances::free_balance(s.child_curator), Balances::minimum_balance());

		// Given: child-bounty with curator and status `RefundAttempted` and payment succeeds
		let s = create_canceled_child_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When
		assert_ok!(Bounties::check_status(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::BountyRefundProcessed {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id)
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id),
			None
		);
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 0);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			1
		);
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			0
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::iter_prefix(s.parent_bounty_id).count(),
			0
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);
		assert!(Preimage::is_requested(&s.metadata)); // still requested by parent bounty
		assert_eq!(
			Balances::free_balance(s.child_curator),
			Balances::minimum_balance() + s.child_curator_deposit
		); // initial

		// Given: child-bounty without curator and status `RefundAttempted` and payment succeeds
		let s = create_active_parent_bounty();
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			None,
		));
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, s.child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		approve_payment(
			child_bounty_account,
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.asset_kind,
			s.child_value,
		);
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id),
		));
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);
		assert_eq!(Balances::free_balance(s.curator), Balances::minimum_balance()); // reserved

		// When
		assert_ok!(Bounties::check_status(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert!(Preimage::is_requested(&s.metadata)); // still requested by parent bounty
		assert_eq!(Balances::free_balance(s.curator), Balances::minimum_balance()); // still reserved

		// Given: child-bounty status is `PayoutAttempted` and payment succeeds
		let s = create_awarded_child_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When
		assert_ok!(Bounties::check_status(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		expect_events(vec![BountiesEvent::BountyPayoutProcessed {
			index: s.parent_bounty_id,
			child_index: Some(s.child_bounty_id),
			asset_kind: s.asset_kind,
			value: s.child_value,
			beneficiary: s.child_beneficiary,
		}]);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::iter_prefix(s.parent_bounty_id).count(),
			0
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id),
			None
		);
		assert!(Preimage::is_requested(&s.metadata)); // still requested by parent bounty
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 0);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			1
		);
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			s.child_value
		);
		assert_eq!(
			Balances::free_balance(s.child_curator),
			Balances::minimum_balance() + s.child_curator_deposit
		); // initial

		// Given: award same parent bounty as previous `Given` setup
		assert_ok!(Bounties::award_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None,
			s.beneficiary
		));

		// When
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then: BountyPayoutProcessed should emit the net payout (parent value minus child
		// value), not the full parent value.
		let expected_payout = s.value - s.child_value;
		expect_events(vec![BountiesEvent::BountyPayoutProcessed {
			index: s.parent_bounty_id,
			child_index: None,
			asset_kind: s.asset_kind,
			value: expected_payout,
			beneficiary: s.beneficiary,
		}]);
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			0
		);
		assert!(!Preimage::is_requested(&s.metadata)); // no longer requested
	});
}

#[test]
fn check_status_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_parent_bounty();

		// When/Then
		assert_noop!(Bounties::check_status(RuntimeOrigin::none(), 2, None), BadOrigin);

		// When/Then
		assert_noop!(
			Bounties::check_status(RuntimeOrigin::signed(1), 2, None),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, Some(2)),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::FundingInconclusive
		);

		// Given
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Failure);
		assert_ok!(Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		assert_noop!(
			Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// Given
		let s = create_canceled_parent_bounty();

		// Then
		assert_noop!(
			Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::RefundInconclusive
		);

		// Given
		let s = create_awarded_parent_bounty();

		// Then
		assert_noop!(
			Bounties::check_status(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::PayoutInconclusive
		);
	});
}

#[test]
fn retry_payment_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status `FundingAttempted`
		let s = create_parent_bounty();
		let parent_bounty_account =
			Bounties::bounty_account(s.parent_bounty_id, s.asset_kind).expect("conversion failed");
		reject_payment(parent_bounty_account, s.parent_bounty_id, None, s.asset_kind, s.value);

		// When
		assert_ok!(Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(paid(parent_bounty_account, s.asset_kind), s.value);
		assert_eq!(
			last_event(),
			BountiesEvent::Paid { index: s.parent_bounty_id, child_index: None, payment_id }
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::FundingAttempted {
					curator: s.curator,
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);

		// Given: parent bounty status `RefundAttempted`
		let s = create_canceled_parent_bounty();
		let funding_source_account =
			Bounties::funding_source_account(s.asset_kind).expect("conversion failed");
		reject_payment(funding_source_account, s.parent_bounty_id, None, s.asset_kind, s.value);

		// When
		assert_ok!(Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(paid(funding_source_account, s.asset_kind), s.value);
		assert_eq!(
			last_event(),
			BountiesEvent::Paid { index: s.parent_bounty_id, child_index: None, payment_id }
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::RefundAttempted {
				curator: Some(s.curator),
				payment_status: PaymentState::Attempted { id: payment_id }
			},
		);

		// Given: parent bounty status `PayoutAttempted`
		let s = create_awarded_parent_bounty();
		reject_payment(s.beneficiary, s.parent_bounty_id, None, s.asset_kind, s.value);

		// When
		assert_ok!(Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(paid(s.beneficiary, s.asset_kind), s.value);
		assert_eq!(
			last_event(),
			BountiesEvent::Paid { index: s.parent_bounty_id, child_index: None, payment_id }
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::PayoutAttempted {
				curator: s.curator,
				beneficiary: s.beneficiary,
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);

		// Given: child-bounty status `FundingAttempted`
		let s = create_child_bounty_with_curator();
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, s.child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		reject_payment(
			child_bounty_account,
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.asset_kind,
			s.child_value,
		);

		// When
		assert_ok!(Bounties::retry_payment(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(paid(child_bounty_account, s.asset_kind), s.child_value);
		assert_eq!(
			last_event(),
			BountiesEvent::Paid {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::FundingAttempted {
					curator: s.child_curator,
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);

		// Given: child-bounty status `RefundAttempted`
		let s = create_canceled_child_bounty();
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, s.child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		let parent_bounty_account =
			Bounties::bounty_account(s.parent_bounty_id, s.asset_kind).expect("conversion failed");
		reject_payment(
			parent_bounty_account,
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.asset_kind,
			s.child_value,
		);

		// When
		assert_ok!(Bounties::retry_payment(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(paid(child_bounty_account, s.asset_kind), s.child_value);
		assert_eq!(
			last_event(),
			BountiesEvent::Paid {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::RefundAttempted {
				curator: Some(s.child_curator),
				payment_status: PaymentState::Attempted { id: payment_id }
			},
		);

		// Given: child-bounty status `PayoutAttempted`
		let s = create_awarded_child_bounty();
		reject_payment(
			s.child_beneficiary,
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.asset_kind,
			s.child_value,
		);

		// When
		assert_ok!(Bounties::retry_payment(
			RuntimeOrigin::signed(1),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(paid(s.child_beneficiary, s.asset_kind), s.child_value);
		expect_events(vec![BountiesEvent::Paid {
			index: s.parent_bounty_id,
			child_index: Some(s.child_bounty_id),
			payment_id,
		}]);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::PayoutAttempted {
				curator: s.child_curator,
				beneficiary: s.child_beneficiary,
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);
	});
}

#[test]
fn retry_payment_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::none(), s.parent_bounty_id, None),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::signed(1), 2, None),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, Some(1)),
			Error::<Test>::InvalidIndex
		);

		// Given: parent bounty status is `FundingAttempted` and payment succeeds
		let s = create_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// Given: parent bounty status is `RefundAttempted` and payment succeeds
		let s = create_canceled_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// Given: parent bounty status is `PayoutAttempted` and payments succeed
		let s = create_awarded_parent_bounty();
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// Given: child-bounty status is `FundingAttempted` and payment succeeds
		let s = create_child_bounty_with_curator();
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		set_status(payment_id, PaymentStatus::Success);

		// When/Then
		assert_noop!(
			Bounties::retry_payment(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			Error::<Test>::UnexpectedStatus
		);
	});
}

#[test]
fn accept_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status is `Funded`
		let s = create_funded_parent_bounty();

		// When
		assert_ok!(Bounties::accept_curator(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None,
		));

		// Then
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::Active { curator: s.curator },
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(s.value)
		);
		assert_eq!(Balances::reserved_balance(&s.curator), s.curator_deposit);

		// Given: child-bounty status is `Funded`
		let s = create_funded_child_bounty();

		// When
		assert_ok!(Bounties::accept_curator(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id),
		));

		// Then
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::Active { curator: s.child_curator },
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			)
			.unwrap(),
			consideration(s.child_value)
		);
		assert_eq!(Balances::reserved_balance(&s.child_curator), s.child_curator_deposit);

		// Given: 2nd child-bounty with same curator
		let _ = Balances::mint_into(&s.child_curator, s.child_curator_deposit);
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			Some(s.child_curator),
		));
		let child_bounty_id =
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id) - 1;
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		approve_payment(
			child_bounty_account,
			s.parent_bounty_id,
			Some(child_bounty_id),
			s.asset_kind,
			s.child_value * 2,
		);

		// When
		assert_ok!(Bounties::accept_curator(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(child_bounty_id),
		));
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			)
			.unwrap(),
			consideration(s.child_value)
		);
		assert_eq!(Balances::reserved_balance(&s.child_curator), s.child_curator_deposit * 2);
	});
}

#[test]
fn accept_curator_handles_different_deposit_calculations() {
	ExtBuilder::default().build_and_execute(|| {
		// Given case 1: Lower bound
		let curator = 2;
		let parent_bounty_id = 0;
		let asset_kind = 1;
		let value = 2;
		let metadata = note_preimage(1);
		let parent_bounty_account =
			Bounties::bounty_account(parent_bounty_id, asset_kind).expect("conversion failed");
		Balances::make_free_balance_be(&curator, 100);
		assert_ok!(Bounties::fund_bounty(
			RuntimeOrigin::root(),
			Box::new(asset_kind),
			value,
			curator,
			metadata
		));
		approve_payment(parent_bounty_account, parent_bounty_id, None, asset_kind, value);

		// When
		assert_ok!(Bounties::accept_curator(
			RuntimeOrigin::signed(curator),
			parent_bounty_id,
			None,
		));

		// Then
		let expected_deposit = CuratorDepositMin::get();
		assert_eq!(Balances::free_balance(&curator), 100 - expected_deposit);
		assert_eq!(Balances::reserved_balance(&curator), expected_deposit);

		// Given case 2: Upper bound
		let curator = 3;
		let parent_bounty_id = 1;
		let value = 1_000_000;
		let starting_balance = value * 2;
		let parent_bounty_account =
			Bounties::bounty_account(parent_bounty_id, asset_kind).expect("conversion failed");
		Balances::make_free_balance_be(&curator, starting_balance);
		SpendLimit::set(value); // Allow for a larger spend limit:
		assert_ok!(Bounties::fund_bounty(
			RuntimeOrigin::root(),
			Box::new(asset_kind),
			value,
			curator,
			metadata
		));
		approve_payment(parent_bounty_account, parent_bounty_id, None, asset_kind, value);

		// When
		assert_ok!(Bounties::accept_curator(
			RuntimeOrigin::signed(curator),
			parent_bounty_id,
			None,
		));

		// Then
		let expected_deposit = CuratorDepositMax::get();
		assert_eq!(Balances::free_balance(&curator), starting_balance - expected_deposit);
		assert_eq!(Balances::reserved_balance(&curator), expected_deposit);
	});
}

#[test]
fn accept_curator_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(s.curator), s.parent_bounty_id, None,),
			Error::<Test>::UnexpectedStatus
		);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::none(), s.parent_bounty_id, None,),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(s.curator), 2, None),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(
				RuntimeOrigin::signed(s.child_curator),
				s.parent_bounty_id,
				Some(2),
			),
			Error::<Test>::InvalidIndex
		);

		// Given: parent bounty status is `Funded`
		Balances::make_free_balance_be(&s.curator, s.curator_deposit - 1);
		let parent_bounty_account =
			Bounties::bounty_account(s.parent_bounty_id, s.asset_kind).expect("conversion failed");
		approve_payment(parent_bounty_account, s.parent_bounty_id, None, s.asset_kind, s.value);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(1), s.parent_bounty_id, None,),
			Error::<Test>::RequireCurator
		);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(RuntimeOrigin::signed(s.curator), s.parent_bounty_id, None,),
			TokenError::FundsUnavailable
		);

		// Given: child-bounty status is `Funded`
		let s = create_child_bounty_with_curator();
		Balances::make_free_balance_be(&s.child_curator, s.child_curator_deposit - 1);
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, s.child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		approve_payment(
			child_bounty_account,
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.asset_kind,
			s.child_value,
		);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
			),
			Error::<Test>::RequireCurator
		);

		// When/Then
		assert_noop!(
			Bounties::accept_curator(
				RuntimeOrigin::signed(s.child_curator),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
			),
			TokenError::FundsUnavailable
		);
	});
}

#[test]
fn unassign_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status is `Funded`
		let s = create_funded_parent_bounty();

		// When: sender is the curator
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None
		));

		// Then
		expect_events(vec![BountiesEvent::CuratorUnassigned {
			index: s.parent_bounty_id,
			child_index: None,
		}]);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::CuratorUnassigned,
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);
		assert_eq!(
			Balances::free_balance(&s.curator),
			Balances::minimum_balance() + s.curator_deposit
		); // not burned

		// Given: parent bounty status is `Funded`
		let s = create_funded_parent_bounty();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::root(), s.parent_bounty_id, None));

		// Then
		assert_eq!(
			Balances::free_balance(&s.curator),
			Balances::minimum_balance() + s.curator_deposit
		); // not burned

		// Given: parent bounty status is `Active` and sender is the curator
		let s = create_active_parent_bounty();

		// When: sender is the curator
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None
		));

		// Then
		assert_eq!(
			Balances::free_balance(&s.curator),
			Balances::minimum_balance() + s.curator_deposit
		); // not burned

		// Given: parent bounty status is `Active` and sender is `RejectOrigin`
		let s = create_active_parent_bounty();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::unassign_curator(RuntimeOrigin::root(), s.parent_bounty_id, None));

		// Then
		assert_eq!(Balances::free_balance(&s.curator), Balances::minimum_balance()); // burned

		// Given: child-bounty status is `Funded`
		let s = create_funded_child_bounty();

		// When: sender is the child curator
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		expect_events(vec![BountiesEvent::CuratorUnassigned {
			index: s.parent_bounty_id,
			child_index: Some(s.child_bounty_id),
		}]);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::CuratorUnassigned,
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);
		assert_eq!(
			Balances::free_balance(&s.child_curator),
			Balances::minimum_balance() + s.child_curator_deposit
		); // not burned

		// Given: child-bounty status is `Funded`
		let s = create_funded_child_bounty();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::root(),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(
			Balances::free_balance(&s.child_curator),
			Balances::minimum_balance() + s.child_curator_deposit
		); // not burned

		// Given: child-bounty status is `Active`
		let s = create_active_child_bounty();

		// When: sender is child curator
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(
			Balances::free_balance(&s.child_curator),
			Balances::minimum_balance() + s.child_curator_deposit
		); // not burned

		// Given: child-bounty status is `Active`
		let s = create_active_child_bounty();

		// When: sender is parent curator
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		assert_eq!(Balances::free_balance(&s.child_curator), Balances::minimum_balance());
		// burned
	});
}

#[test]
fn unassign_curator_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = setup_bounty();
		assert_ok!(Bounties::fund_bounty(
			RuntimeOrigin::root(),
			Box::new(s.asset_kind),
			s.value,
			s.curator,
			s.metadata
		));

		// When/Then
		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::root(), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// Given
		let s = create_funded_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::none(), s.parent_bounty_id, None),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::root(), 2, None),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::root(), s.parent_bounty_id, Some(1)),
			Error::<Test>::InvalidIndex
		);

		// Given
		let s = create_active_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::unassign_curator(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			BadOrigin
		);
	});
}

#[test]
fn propose_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status `UnassignedCurator`
		let s = create_parent_bounty_with_unassigned_curator();

		// When
		assert_ok!(Bounties::propose_curator(
			RuntimeOrigin::root(),
			s.parent_bounty_id,
			None,
			s.curator,
		));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::CuratorProposed {
				index: s.parent_bounty_id,
				child_index: None,
				curator: s.curator
			}
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::Funded { curator: s.curator },
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);

		// Given: child-bounty status `UnassignedCurator`
		let s = create_child_bounty_with_unassigned_curator();

		// When
		assert_ok!(Bounties::propose_curator(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.child_curator,
		));

		// Then
		assert_eq!(
			last_event(),
			BountiesEvent::CuratorProposed {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				curator: s.child_curator
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::Funded { curator: s.child_curator },
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);
	});
}

#[test]
fn propose_curator_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status `Funded`
		let s = create_funded_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::propose_curator(RuntimeOrigin::root(), s.parent_bounty_id, None, s.curator,),
			Error::<Test>::UnexpectedStatus
		);

		// Given: parent bounty status `UnassignedCurator`
		let s = create_parent_bounty_with_unassigned_curator();

		// When/Then
		assert_noop!(
			Bounties::propose_curator(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				None,
				s.curator,
			),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::propose_curator(RuntimeOrigin::root(), 3, None, s.curator),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		SpendLimit::set(s.value - 1);
		assert_noop!(
			Bounties::propose_curator(RuntimeOrigin::root(), s.parent_bounty_id, None, s.curator,),
			Error::<Test>::InsufficientPermission
		);

		// Given: child-bounty status `Funded`
		SpendLimit::set(s.value);
		let s = create_active_parent_bounty();
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			Some(s.child_curator),
		));

		// When/Then
		assert_noop!(
			Bounties::propose_curator(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
				s.child_curator,
			),
			Error::<Test>::UnexpectedStatus
		);

		// Given: child-bounty status `UnassignedCurator`
		let s = create_child_bounty_with_unassigned_curator();

		// When/Then
		assert_noop!(
			Bounties::propose_curator(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
				s.child_curator,
			),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::propose_curator(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				Some(3),
				s.child_curator,
			),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::propose_curator(
				RuntimeOrigin::root(),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
				s.child_curator,
			),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::propose_curator(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
				s.child_curator,
			),
			BadOrigin
		);
	});
}

#[docify::export]
#[test]
fn award_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status `Active`
		let s = create_active_parent_bounty();

		// When
		assert_ok!(Bounties::award_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None,
			s.beneficiary
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::PayoutAttempted {
					curator: s.curator,
					beneficiary: s.beneficiary,
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(s.value)
		);

		expect_events(vec![
			BountiesEvent::Paid { index: s.parent_bounty_id, child_index: None, payment_id },
			BountiesEvent::BountyAwarded {
				index: s.parent_bounty_id,
				child_index: None,
				beneficiary: s.beneficiary,
			},
		]);

		// Given: child-bounty status `Active`
		let s = create_active_child_bounty();

		// When
		assert_ok!(Bounties::award_bounty(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.child_beneficiary
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::PayoutAttempted {
					curator: s.child_curator,
					beneficiary: s.child_beneficiary,
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			)
			.unwrap(),
			consideration(s.child_value)
		);
		expect_events(vec![
			BountiesEvent::Paid {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				payment_id,
			},
			BountiesEvent::BountyAwarded {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id),
				beneficiary: s.child_beneficiary,
			},
		]);
	});
}

#[test]
fn award_bounty_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_funded_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::award_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				None,
				s.beneficiary
			),
			Error::<Test>::UnexpectedStatus
		);

		// Given
		let s = create_active_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::award_bounty(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				None,
				s.beneficiary
			),
			Error::<Test>::RequireCurator
		);

		// When/Then
		assert_noop!(
			Bounties::award_bounty(RuntimeOrigin::signed(s.curator), 3, None, s.beneficiary),
			Error::<Test>::InvalidIndex
		);

		// Given
		let s = create_active_child_bounty();

		// When/Then
		assert_noop!(
			Bounties::award_bounty(
				RuntimeOrigin::signed(s.child_curator),
				s.parent_bounty_id,
				Some(3),
				s.child_beneficiary
			),
			Error::<Test>::InvalidIndex
		);
	})
}

#[test]
fn close_bounty_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: parent bounty status is `Funded`
		let s = create_funded_parent_bounty();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None,));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(
			last_event(),
			BountiesEvent::BountyCanceled { index: s.parent_bounty_id, child_index: None }
		);
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap(),
			Bounty {
				asset_kind: s.asset_kind,
				value: s.value,
				metadata: s.metadata,
				status: BountyStatus::RefundAttempted {
					curator: Some(s.curator),
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>),
			None
		);

		// Given: parent bounty status is `Funded`
		let s = create_funded_parent_bounty();

		// When: sender is parent curator
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None,
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::RefundAttempted {
				curator: Some(s.curator),
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);

		// Given: parent bounty status is `UnassignedCurator`
		let s = create_parent_bounty_with_unassigned_curator();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None,));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, None).expect("no payment attempt");
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().status,
			BountyStatus::RefundAttempted {
				curator: None,
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);

		// Given: child-bounty status is `Funded`
		let s = create_funded_child_bounty();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::root(),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(
			last_event(),
			BountiesEvent::BountyCanceled {
				index: s.parent_bounty_id,
				child_index: Some(s.child_bounty_id)
			}
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap(),
			ChildBounty {
				parent_bounty: s.parent_bounty_id,
				value: s.child_value,
				metadata: s.metadata,
				status: BountyStatus::RefundAttempted {
					curator: Some(s.child_curator),
					payment_status: PaymentState::Attempted { id: payment_id }
				},
			}
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			None
		);

		// Given: child-bounty status is `Funded`
		let s = create_funded_child_bounty();

		// When: sender is curator
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::RefundAttempted {
				curator: Some(s.child_curator),
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);

		// Given: child-bounty status is `Funded`
		let s = create_funded_child_bounty();

		// When: sender is parent curator
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::RefundAttempted {
				curator: Some(s.child_curator),
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);

		// Given: child-bounty status is `UnassignedCurator`
		let s = create_child_bounty_with_unassigned_curator();

		// When: sender is `RejectOrigin`
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::root(),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));

		// Then
		let payment_id = get_payment_id(s.parent_bounty_id, Some(s.child_bounty_id))
			.expect("no payment attempt");
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id)
				.unwrap()
				.status,
			BountyStatus::RefundAttempted {
				curator: None,
				payment_status: PaymentState::Attempted { id: payment_id }
			}
		);
	})
}

#[test]
fn close_bounty_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None),
			Error::<Test>::UnexpectedStatus
		);

		// Given
		let s = create_parent_bounty_with_unassigned_curator();

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::signed(s.curator), s.parent_bounty_id, None),
			BadOrigin
		);

		// Given
		let s = create_active_parent_bounty();

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::none(), s.parent_bounty_id, None),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::signed(1), s.parent_bounty_id, None),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), 3, None),
			Error::<Test>::InvalidIndex
		);

		// Given
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			Some(s.child_curator),
		));

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None),
			Error::<Test>::HasActiveChildBounty
		);

		// Given
		let s = create_active_child_bounty();

		// When/Then
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, Some(2)),
			Error::<Test>::InvalidIndex
		);

		// When/Then
		assert_noop!(
			Bounties::close_bounty(
				RuntimeOrigin::signed(1),
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			BadOrigin
		);

		// When/Then
		assert_noop!(
			Bounties::close_bounty(
				RuntimeOrigin::none(),
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			BadOrigin
		);

		// Given
		let s = create_child_bounty_with_unassigned_curator();

		// When/Then
		assert_noop!(
			Bounties::close_bounty(
				RuntimeOrigin::signed(s.child_curator),
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			),
			BadOrigin
		);
	})
}

#[test]
fn close_parent_with_child_bounty() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_active_child_bounty();

		// When/Then: close parent bounty fails
		assert_noop!(
			Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None),
			Error::<Test>::HasActiveChildBounty
		);

		// Given: close child bounty
		assert_ok!(Bounties::close_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		));
		let parent_bounty_account =
			Bounties::bounty_account(s.parent_bounty_id, s.asset_kind).expect("conversion failed");
		approve_payment(
			parent_bounty_account,
			s.parent_bounty_id,
			Some(s.child_bounty_id),
			s.asset_kind,
			s.value + s.child_value, // parent bounty value + child bounty value
		);
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 0);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			1
		);

		// When
		assert_ok!(Bounties::close_bounty(RuntimeOrigin::root(), s.parent_bounty_id, None));
		let funding_source_account =
			Bounties::funding_source_account(s.asset_kind).expect("conversion failed");
		approve_payment(funding_source_account, s.parent_bounty_id, None, s.asset_kind, s.value);

		// Then
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			0
		);
	});
}

#[test]
fn fund_and_award_child_bounty_without_curator_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given
		let s = create_active_parent_bounty();

		// When
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.child_value,
			s.metadata,
			None,
		));
		let child_bounty_id = 0;
		let child_bounty_account =
			Bounties::child_bounty_account(s.parent_bounty_id, child_bounty_id, s.asset_kind)
				.expect("conversion failed");
		approve_payment(
			child_bounty_account,
			s.parent_bounty_id,
			Some(child_bounty_id),
			s.asset_kind,
			s.child_value,
		);
		assert_ok!(Bounties::award_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			Some(child_bounty_id),
			s.child_beneficiary
		));
		approve_payment(
			s.child_beneficiary,
			s.parent_bounty_id,
			Some(child_bounty_id),
			s.asset_kind,
			s.child_value,
		);

		// Then
		expect_events(vec![BountiesEvent::BountyPayoutProcessed {
			index: s.parent_bounty_id,
			child_index: Some(s.child_bounty_id),
			asset_kind: s.asset_kind,
			value: s.child_value,
			beneficiary: s.child_beneficiary,
		}]);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::iter_prefix(s.parent_bounty_id).count(),
			0
		);
		assert_eq!(
			pallet_bounties::ChildBounties::<Test>::get(s.parent_bounty_id, s.child_bounty_id),
			None
		);
		assert_eq!(pallet_bounties::ChildBountiesPerParent::<Test>::get(s.parent_bounty_id), 0);
		assert_eq!(
			pallet_bounties::TotalChildBountiesPerParent::<Test>::get(s.parent_bounty_id),
			1
		);
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			s.child_value
		);
		assert_eq!(
			Balances::free_balance(s.child_curator),
			Balances::minimum_balance() + s.child_curator_deposit
		); // initial
	})
}

#[test]
fn unprivileged_caller_cannot_unassign_active_child_curator_when_parent_not_active() {
	ExtBuilder::default().build_and_execute(|| {
		let s = create_active_child_bounty();
		let attacker: u128 = 99;

		// Verify preconditions: child bounty is Active with curator deposit held.
		assert_eq!(Balances::reserved_balance(&s.child_curator), s.child_curator_deposit);
		assert!(pallet_bounties::CuratorDeposit::<Test>::get(
			s.parent_bounty_id,
			Some(s.child_bounty_id)
		)
		.is_some());

		// Step 1: Parent curator voluntarily unassigns from parent bounty.
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None,
		));

		// Parent is now CuratorUnassigned; child is still Active.
		let (_, _, _, parent_status, _) =
			Bounties::get_bounty_details(s.parent_bounty_id, None).expect("parent bounty exists");
		assert_eq!(parent_status, BountyStatus::CuratorUnassigned);

		let (_, _, _, child_status, parent_curator) =
			Bounties::get_bounty_details(s.parent_bounty_id, Some(s.child_bounty_id))
				.expect("child bounty exists");
		assert!(matches!(child_status, BountyStatus::Active { .. }));
		assert!(
			parent_curator.is_none(),
			"parent_curator should be None since parent is not Active"
		);

		// Step 2: An unprivileged attacker tries to unassign the active child bounty curator.
		// This must be rejected with BadOrigin.
		assert_noop!(
			Bounties::unassign_curator(
				RuntimeOrigin::signed(attacker),
				s.parent_bounty_id,
				Some(s.child_bounty_id),
			),
			BadOrigin
		);

		// Child bounty remains Active the unprivileged caller had no effect.
		let (_, _, _, child_status, _) =
			Bounties::get_bounty_details(s.parent_bounty_id, Some(s.child_bounty_id))
				.expect("child bounty exists");
		assert!(matches!(child_status, BountyStatus::Active { .. }));

		// Curator deposit is still in storage and the hold is intact.
		assert!(
			pallet_bounties::CuratorDeposit::<Test>::get(
				s.parent_bounty_id,
				Some(s.child_bounty_id)
			)
			.is_some(),
			"curator deposit must remain in storage"
		);
		assert_eq!(
			Balances::reserved_balance(&s.child_curator),
			s.child_curator_deposit,
			"curator deposit hold must remain intact"
		);

		// Step 3: Verify that the child curator can still voluntarily unassign themselves.
		assert_ok!(Bounties::unassign_curator(
			RuntimeOrigin::signed(s.child_curator),
			s.parent_bounty_id,
			Some(s.child_bounty_id),
		));

		let (_, _, _, child_status, _) =
			Bounties::get_bounty_details(s.parent_bounty_id, Some(s.child_bounty_id))
				.expect("child bounty exists");
		assert_eq!(child_status, BountyStatus::CuratorUnassigned);

		// Curator's deposit was properly released.
		assert_eq!(
			Balances::reserved_balance(&s.child_curator),
			0,
			"curator deposit hold must be released after voluntary unassign"
		);
	});
}

#[test]
fn multi_asset_bounty_accounts_differ_from_legacy_bounty_accounts() {
	ExtBuilder::default().build_and_execute(|| {
		use sp_runtime::traits::AccountIdConversion;

		let bounty_id: BountyIndex = 0;

		// Old derivation (what pallet-bounties uses with &str)
		let old_bounty_account: u128 =
			BountyPalletId::get().into_sub_account_truncating(("bt", bounty_id));
		// New derivation (what multi-asset-bounties uses with [u8; 3])
		let new_bounty_account: u128 = BountyPalletId::get()
			.into_sub_account_truncating((BountyAccountPrefix::get(), bounty_id));

		assert_ne!(
			old_bounty_account, new_bounty_account,
			"multi-asset bounty account must differ from legacy bounty account"
		);

		let parent_bounty_id: BountyIndex = 0;
		let child_bounty_id: BountyIndex = 0;

		// Old derivation (what pallet-child-bounties uses with &str)
		let old_child_account: u128 = BountyPalletId::get().into_sub_account_truncating((
			"cb",
			parent_bounty_id,
			child_bounty_id,
		));
		// New derivation (what multi-asset-bounties uses with [u8; 3])
		let new_child_account: u128 = BountyPalletId::get().into_sub_account_truncating((
			ChildBountyAccountPrefix::get(),
			parent_bounty_id,
			child_bounty_id,
		));

		assert_ne!(
			old_child_account, new_child_account,
			"multi-asset child bounty account must differ from legacy child bounty account"
		);

		// Also verify bounty and child-bounty accounts are distinct from each other
		// when using the same indices
		assert_ne!(
			new_bounty_account, new_child_account,
			"bounty and child-bounty accounts must differ even with the same indices"
		);
	});
}

#[test]
fn integrity_test() {
	ExtBuilder::default().build_and_execute(|| {
		Bounties::integrity_test();
	});
}

#[test]
fn increase_value_works() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: an active parent bounty (value 50, curator deposit 25 held).
		let s = create_active_parent_bounty();
		assert_eq!(Balances::reserved_balance(s.curator), s.curator_deposit);
		let increase = 20;
		let new_value = s.value + increase; // 70

		// Give the curator enough free balance to cover the larger deposit hold.
		let _ = Balances::mint_into(&s.curator, 100);

		// When
		assert_ok!(Bounties::increase_value(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			increase,
		));

		// Then: the recorded value is raised and the event is emitted.
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().value,
			new_value
		);
		expect_events(vec![BountiesEvent::BountyValueIncreased {
			index: s.parent_bounty_id,
			old_value: s.value,
			new_value,
		}]);

		// And: the curator deposit was rescaled to the new value (50% * 70 = 35).
		let expected_deposit = new_value / 2; // 50% multiplier, within [Min, Max]
		assert_eq!(Balances::reserved_balance(s.curator), expected_deposit);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(new_value)
		);
	});
}

#[test]
fn increase_value_fails() {
	ExtBuilder::default().build_and_execute(|| {
		// When/Then: unknown bounty index.
		assert_noop!(
			Bounties::increase_value(RuntimeOrigin::signed(4), 99, 10),
			Error::<Test>::InvalidIndex
		);

		// Given: a parent bounty that is not yet `Active` (`FundingAttempted`).
		let s = create_parent_bounty();

		// When/Then: cannot increase value before the bounty is active.
		assert_noop!(
			Bounties::increase_value(RuntimeOrigin::signed(s.curator), s.parent_bounty_id, 10),
			Error::<Test>::UnexpectedStatus
		);

		// Given: an active parent bounty.
		let s = create_active_parent_bounty();

		// When/Then: unsigned origin is rejected.
		assert_noop!(
			Bounties::increase_value(RuntimeOrigin::none(), s.parent_bounty_id, 10),
			BadOrigin
		);

		// When/Then: only the curator may increase the value.
		assert_noop!(
			Bounties::increase_value(RuntimeOrigin::signed(1), s.parent_bounty_id, 10),
			Error::<Test>::RequireCurator
		);

		// When/Then: a zero increase is rejected.
		assert_noop!(
			Bounties::increase_value(RuntimeOrigin::signed(s.curator), s.parent_bounty_id, 0),
			Error::<Test>::InvalidValue
		);
	});
}

#[test]
fn increase_value_then_award_pays_new_value() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: an active parent bounty, value increased from 50 to 70.
		let s = create_active_parent_bounty();
		let _ = Balances::mint_into(&s.curator, 100);
		let increase = 20;
		let new_value = s.value + increase;
		assert_ok!(Bounties::increase_value(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			increase,
		));

		// When: the bounty is awarded and the payout settles.
		assert_ok!(Bounties::award_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			None,
			s.beneficiary,
		));
		approve_payment(s.beneficiary, s.parent_bounty_id, None, s.asset_kind, new_value);

		// Then: the beneficiary is paid the NEW value, not the original.
		expect_events(vec![BountiesEvent::BountyPayoutProcessed {
			index: s.parent_bounty_id,
			child_index: None,
			asset_kind: s.asset_kind,
			value: new_value,
			beneficiary: s.beneficiary,
		}]);
		assert_eq!(paid(s.beneficiary, s.asset_kind), new_value);
	});
}

#[test]
fn increase_value_creates_child_bounty_headroom() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: an active parent bounty (value 50) with its entire value allocated to one child.
		let s = create_active_parent_bounty();
		let _ = Balances::mint_into(&s.curator, 100);
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			s.value, // 50 — consumes all parent value
			s.metadata,
			None,
		));
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			s.value
		);

		// When/Then: no headroom left, another child is rejected.
		assert_noop!(
			Bounties::fund_child_bounty(
				RuntimeOrigin::signed(s.curator),
				s.parent_bounty_id,
				1,
				s.metadata,
				None,
			),
			Error::<Test>::InsufficientBountyValue
		);

		// Given: the parent value is increased, creating new headroom.
		let increase = 30;
		assert_ok!(Bounties::increase_value(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			increase,
		));

		// When/Then: a child funded from the new headroom now succeeds, and the cumulative child
		// value equals the new parent value (invariant Σ child ≤ parent preserved).
		assert_ok!(Bounties::fund_child_bounty(
			RuntimeOrigin::signed(s.curator),
			s.parent_bounty_id,
			increase, // 30
			s.metadata,
			None,
		));
		assert_eq!(
			pallet_bounties::ChildBountiesValuePerParent::<Test>::get(s.parent_bounty_id),
			s.value + increase, // 80
		);
	});
}

#[test]
fn increase_value_reverts_when_curator_cannot_fund_deposit() {
	ExtBuilder::default().build_and_execute(|| {
		// Given: an active parent bounty. The curator's free balance is only the existential
		// deposit (their 25 deposit is already held), so they cannot fund a larger hold.
		let s = create_active_parent_bounty();
		assert_eq!(Balances::reserved_balance(s.curator), s.curator_deposit);

		// When/Then: the increase requires a larger deposit (50% * 70 = 35, i.e. +10) the curator
		// cannot afford, so the call fails.
		assert_noop!(
			Bounties::increase_value(RuntimeOrigin::signed(s.curator), s.parent_bounty_id, 20),
			TokenError::FundsUnavailable
		);

		// And: the whole call is rolled back — the deposit was NOT orphaned by the earlier `take`,
		// and the value is unchanged.
		assert_eq!(
			pallet_bounties::Bounties::<Test>::get(s.parent_bounty_id).unwrap().value,
			s.value
		);
		assert_eq!(
			pallet_bounties::CuratorDeposit::<Test>::get(s.parent_bounty_id, None::<BountyIndex>)
				.unwrap(),
			consideration(s.value)
		);
		assert_eq!(Balances::reserved_balance(s.curator), s.curator_deposit);
	});
}
