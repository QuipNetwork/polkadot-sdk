// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use super::*;

use crate::{
	initializer::SessionChangeNotification,
	mock::{
		assert_last_event, new_test_ext, Balances, BrokerId, Coretime, OnDemand, Paras,
		ParasShared, RuntimeEvent, RuntimeOrigin, Scheduler, System, Test,
	},
	on_demand::{self, mock_helpers::GenesisConfigBuilder, Error},
	origin::Origin as ParachainOrigin,
	paras::{ParaGenesisArgs, ParaKind},
};
use frame_support::{assert_noop, assert_ok};
use pallet_balances::Error as BalancesError;
use polkadot_primitives::{BlockNumber, SessionIndex, ValidationCode};
use sp_runtime::traits::BadOrigin;

fn schedule_blank_para(id: ParaId, parakind: ParaKind) {
	let validation_code: ValidationCode = vec![1, 2, 3].into();
	assert_ok!(Paras::schedule_para_initialize(
		id,
		ParaGenesisArgs {
			genesis_head: Vec::new().into(),
			validation_code: validation_code.clone(),
			para_kind: parakind,
		}
	));

	assert_ok!(Paras::add_trusted_validation_code(RuntimeOrigin::root(), validation_code));
}

fn run_to_block(
	to: BlockNumber,
	new_session: impl Fn(BlockNumber) -> Option<SessionChangeNotification<BlockNumber>>,
) {
	while System::block_number() < to {
		let b = System::block_number();

		Scheduler::initializer_finalize();
		Paras::initializer_finalize(b);

		if let Some(notification) = new_session(b + 1) {
			let mut notification_with_session_index = notification;
			// We will make every session change trigger an action queue. Normally this may require
			// 2 or more session changes.
			if notification_with_session_index.session_index == SessionIndex::default() {
				notification_with_session_index.session_index = ParasShared::scheduled_session();
			}
			Paras::initializer_on_new_session(&notification_with_session_index);
			Scheduler::initializer_on_new_session(&notification_with_session_index);
		}

		System::on_finalize(b);

		System::on_initialize(b + 1);
		System::set_block_number(b + 1);

		Paras::initializer_initialize(b + 1);
		Scheduler::initializer_initialize(b + 1);

		// Update the spot traffic and revenue on every block.
		OnDemand::on_initialize(b + 1);

		// In the real runtime this is expected to be called by the `InclusionInherent` pallet.
		Scheduler::advance_claim_queue(|_| false);
	}
}

fn place_order_run_to_blocknumber(para_id: ParaId, blocknumber: Option<BlockNumber>) {
	let alice = 100u64;
	let amt = 10_000_000u128;

	Balances::make_free_balance_be(&alice, amt);

	if let Some(bn) = blocknumber {
		run_to_block(bn, |n| if n == bn { Some(Default::default()) } else { None });
	}
	#[allow(deprecated)]
	OnDemand::place_order_allow_death(RuntimeOrigin::signed(alice), amt, para_id).unwrap()
}

fn place_order(para_id: ParaId) {
	place_order_run_to_blocknumber(para_id, None);
}

/// The origin of the coretime chain, which is the only para allowed to send us order batches.
fn broker_origin() -> RuntimeOrigin {
	RuntimeOrigin::from(ParachainOrigin::Parachain(BrokerId::get().into()))
}

#[test]
fn spot_traffic_capacity_zero_returns_none() {
	match OnDemand::calculate_spot_traffic(
		FixedU128::from(u128::MAX),
		0u32,
		u32::MAX,
		Perbill::from_percent(100),
		Perbill::from_percent(1),
	) {
		Ok(_) => panic!("Error"),
		Err(e) => assert_eq!(e, SpotTrafficCalculationErr::QueueCapacityIsZero),
	};
}

#[test]
fn spot_traffic_queue_size_larger_than_capacity_returns_none() {
	match OnDemand::calculate_spot_traffic(
		FixedU128::from(u128::MAX),
		1u32,
		2u32,
		Perbill::from_percent(100),
		Perbill::from_percent(1),
	) {
		Ok(_) => panic!("Error"),
		Err(e) => assert_eq!(e, SpotTrafficCalculationErr::QueueSizeLargerThanCapacity),
	}
}

#[test]
fn spot_traffic_calculation_identity() {
	match OnDemand::calculate_spot_traffic(
		FixedU128::from_u32(1),
		1000,
		100,
		Perbill::from_percent(10),
		Perbill::from_percent(3),
	) {
		Ok(res) => {
			assert_eq!(res, FixedU128::from_u32(1))
		},
		_ => (),
	}
}

#[test]
fn spot_traffic_calculation_u32_max() {
	match OnDemand::calculate_spot_traffic(
		FixedU128::from_u32(1),
		u32::MAX,
		u32::MAX,
		Perbill::from_percent(100),
		Perbill::from_percent(3),
	) {
		Ok(res) => {
			assert_eq!(res, FixedU128::from_u32(1))
		},
		_ => panic!("Error"),
	};
}

#[test]
fn spot_traffic_calculation_u32_traffic_max() {
	match OnDemand::calculate_spot_traffic(
		FixedU128::from(u128::MAX),
		u32::MAX,
		u32::MAX,
		Perbill::from_percent(1),
		Perbill::from_percent(1),
	) {
		Ok(res) => assert_eq!(res, FixedU128::from(u128::MAX)),
		_ => panic!("Error"),
	};
}

#[test]
fn sustained_target_increases_spot_traffic() {
	let mut traffic = FixedU128::from_u32(1u32);
	for _ in 0..50 {
		traffic = OnDemand::calculate_spot_traffic(
			traffic,
			100,
			12,
			Perbill::from_percent(10),
			Perbill::from_percent(100),
		)
		.unwrap()
	}
	assert_eq!(traffic, FixedU128::from_inner(2_718_103_312_071_174_015u128))
}

#[test]
fn spot_traffic_can_decrease() {
	let traffic = FixedU128::from_u32(100u32);
	match OnDemand::calculate_spot_traffic(
		traffic,
		100u32,
		0u32,
		Perbill::from_percent(100),
		Perbill::from_percent(100),
	) {
		Ok(new_traffic) => {
			assert_eq!(new_traffic, FixedU128::from_inner(50_000_000_000_000_000_000u128))
		},
		_ => panic!("Error"),
	}
}

#[test]
fn spot_traffic_decreases_over_time() {
	let mut traffic = FixedU128::from_u32(100u32);
	for _ in 0..5 {
		traffic = OnDemand::calculate_spot_traffic(
			traffic,
			100u32,
			0u32,
			Perbill::from_percent(100),
			Perbill::from_percent(100),
		)
		.unwrap();
		println!("{traffic}");
	}
	assert_eq!(traffic, FixedU128::from_inner(3_125_000_000_000_000_000u128))
}

#[test]
fn spot_traffic_decreases_between_idle_blocks() {
	// Testing spot traffic assumptions, but using the mock runtime and default on demand
	// configuration values. Ensuring that blocks with no on demand activity (idle)
	// decrease traffic.

	let para_id = ParaId::from(111);

	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		// Initialize the parathread and wait for it to be ready.
		schedule_blank_para(para_id, ParaKind::Parathread);
		assert!(!Paras::is_parathread(para_id));
		run_to_block(100, |n| if n == 100 { Some(Default::default()) } else { None });
		assert!(Paras::is_parathread(para_id));

		// Set the spot traffic to a large number
		OnDemand::set_order_status(OrderStatus {
			traffic: FixedU128::from_u32(10),
			..Default::default()
		});

		assert_eq!(OnDemand::get_order_status().traffic, FixedU128::from_u32(10));

		// Run to block 101 and ensure that the traffic decreases.
		run_to_block(101, |n| if n == 100 { Some(Default::default()) } else { None });
		assert!(OnDemand::get_order_status().traffic < FixedU128::from_u32(10));

		// Run to block 102 and observe that we've hit the default traffic value.
		run_to_block(102, |n| if n == 100 { Some(Default::default()) } else { None });
		assert_eq!(OnDemand::get_order_status().traffic, OnDemand::get_traffic_default_value());
	})
}

#[test]
#[allow(deprecated)]
fn place_order_works() {
	let alice = 1u64;
	let amt = 10_000_000u128;
	let para_id = ParaId::from(111);

	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		// Initialize the parathread and wait for it to be ready.
		schedule_blank_para(para_id, ParaKind::Parathread);

		assert!(!Paras::is_parathread(para_id));

		run_to_block(100, |n| if n == 100 { Some(Default::default()) } else { None });

		assert!(Paras::is_parathread(para_id));

		// Does not work unsigned
		assert_noop!(
			OnDemand::place_order_allow_death(RuntimeOrigin::none(), amt, para_id),
			BadOrigin
		);

		// Does not work with max_amount lower than fee
		let low_max_amt = 1u128;
		assert_noop!(
			OnDemand::place_order_allow_death(RuntimeOrigin::signed(alice), low_max_amt, para_id,),
			Error::<Test>::SpotPriceHigherThanMaxAmount,
		);

		// Does not work with insufficient balance
		assert_noop!(
			OnDemand::place_order_allow_death(RuntimeOrigin::signed(alice), amt, para_id),
			BalancesError::<Test, _>::InsufficientBalance
		);

		// Works
		Balances::make_free_balance_be(&alice, amt);
		run_to_block(101, |n| if n == 101 { Some(Default::default()) } else { None });
		assert_ok!(OnDemand::place_order_allow_death(RuntimeOrigin::signed(alice), amt, para_id));
	});
}

#[test]
#[allow(deprecated)]
fn place_order_keep_alive_keeps_alive() {
	let alice = 1u64;
	let amt = 1u128; // The same as crate::mock's EXISTENTIAL_DEPOSIT
	let max_amt = 10_000_000u128;
	let para_id = ParaId::from(111);

	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let config = configuration::ActiveConfig::<Test>::get();

		// Initialize the parathread and wait for it to be ready.
		schedule_blank_para(para_id, ParaKind::Parathread);
		Balances::make_free_balance_be(&alice, amt);

		assert!(!Paras::is_parathread(para_id));
		run_to_block(100, |n| if n == 100 { Some(Default::default()) } else { None });
		assert!(Paras::is_parathread(para_id));

		assert_noop!(
			OnDemand::place_order_keep_alive(RuntimeOrigin::signed(alice), max_amt, para_id),
			BalancesError::<Test, _>::InsufficientBalance
		);

		Balances::make_free_balance_be(&alice, max_amt);
		assert_ok!(OnDemand::place_order_keep_alive(
			RuntimeOrigin::signed(alice),
			max_amt,
			para_id
		),);

		let spot_price = OnDemand::get_order_status().traffic.saturating_mul_int(
			config.scheduler_params.on_demand_base_fee.saturated_into::<BalanceOf<Test>>(),
		);
		assert_eq!(Balances::free_balance(&alice), max_amt.saturating_sub(spot_price));
	});
}

#[test]
fn place_order_with_credits() {
	let alice = 1u64;
	let initial_credit = 10_000_000u128;
	let para_id = ParaId::from(111);

	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let config = configuration::ActiveConfig::<Test>::get();

		// Initialize the parathread and wait for it to be ready.
		schedule_blank_para(para_id, ParaKind::Parathread);
		OnDemand::credit_account(alice, initial_credit);
		assert_eq!(Credits::<Test>::get(alice), initial_credit);

		assert!(!Paras::is_parathread(para_id));
		let mut current_block = 100;
		run_to_block(current_block, |n| {
			if n == current_block {
				Some(Default::default())
			} else {
				None
			}
		});
		assert!(Paras::is_parathread(para_id));

		let queue_status = OnDemand::get_order_status();
		let spot_price = queue_status.traffic.saturating_mul_int(
			config.scheduler_params.on_demand_base_fee.saturated_into::<BalanceOf<Test>>(),
		);

		// Create an order and pay for it with credits.
		assert_ok!(OnDemand::place_order_with_credits(
			RuntimeOrigin::signed(alice),
			initial_credit,
			para_id
		));
		assert_eq!(Credits::<Test>::get(alice), initial_credit.saturating_sub(spot_price));

		// Async backing:
		current_block += 2;
		assert_eq!(
			OnDemand::peek_order_queue()
				.pop_assignment_for_cores::<Test>(current_block, 1)
				.next(),
			Some(para_id)
		);

		// Insufficient credits:
		Credits::<Test>::insert(alice, 1u128);
		assert_noop!(
			OnDemand::place_order_with_credits(
				RuntimeOrigin::signed(alice),
				1_000_000u128,
				para_id
			),
			Error::<Test>::InsufficientCredits
		);
	});
}

#[test]
fn pop_assignment_for_cores_works() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(110);
		let para_b = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);
		schedule_blank_para(para_b, ParaKind::Parathread);

		let mut block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		// Pop should return none with empty queue
		assert_eq!(OnDemand::pop_assignment_for_cores(block_num, 1).next(), None);

		// Add enough assignments to the order queue.
		for _ in 0..2 {
			place_order_run_to_blocknumber(para_a, Some(block_num));
			place_order(para_b);
			block_num += 1;
		}

		// Go back to where first order became effective:
		let block_num = 11 + 2;

		// Popped assignments should be for the correct paras and cores
		let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 2);
		assert_eq!(assignments.next(), Some(para_a));
		assert_eq!(assignments.next(), Some(para_b));

		let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 2);
		// Should be empty for same block again:
		assert_eq!(assignments.next(), None);

		let mut assignments = OnDemand::pop_assignment_for_cores(block_num + 1, 2);
		assert_eq!(assignments.next(), Some(para_a));
		assert_eq!(assignments.next(), Some(para_b));
	});
}

#[test]
fn affinity_prohibits_parallel_scheduling() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(111);
		let para_b = ParaId::from(222);
		let para_c = ParaId::from(333);

		schedule_blank_para(para_a, ParaKind::Parathread);
		schedule_blank_para(para_b, ParaKind::Parathread);
		schedule_blank_para(para_c, ParaKind::Parathread);

		let mut block_num = 11;

		// 2 cores, 3 orders, a twice:
		{
			place_order_run_to_blocknumber(para_a, Some(block_num));
			place_order(para_a);
			place_order(para_b);

			// Advance 2 for async backing:
			block_num += 2;

			let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 2);
			assert_eq!(assignments.next(), Some(para_a));
			// Next should be `b` ... `a` not allowed:
			assert_eq!(assignments.next(), Some(para_b));
			assert_eq!(assignments.next(), None);
			block_num += 1;

			let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 2);
			// Now we get the second `a`.
			assert_eq!(assignments.next(), Some(para_a));
			assert_eq!(assignments.next(), None);
			block_num += 1;
		}

		// 3 cores, 3 orders, a twice:
		{
			place_order_run_to_blocknumber(para_a, Some(block_num));
			place_order(para_a);
			place_order(para_b);

			block_num += 2;

			let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 3);
			assert_eq!(assignments.next(), Some(para_a));
			// Next should be `b` ... `a` not allowed:
			assert_eq!(assignments.next(), Some(para_b));
			// 3rd should be None, despite having capacity:
			assert_eq!(assignments.next(), None);

			block_num += 1;
			let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 3);
			// Now we get the second `a`.
			assert_eq!(assignments.next(), Some(para_a));
			assert_eq!(assignments.next(), None);
		}

		// 3 cores, 3 orders, no duplicates (sanity check):
		{
			place_order_run_to_blocknumber(para_a, Some(block_num));
			place_order(para_b);
			place_order(para_c);

			block_num += 2;

			let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 3);
			assert_eq!(assignments.next(), Some(para_a));
			// Next should be `b` ... `a` not allowed:
			assert_eq!(assignments.next(), Some(para_b));
			// 3rd should be `c`:
			assert_eq!(assignments.next(), Some(para_c));
			assert_eq!(assignments.next(), None);

			block_num += 1;
			let mut assignments = OnDemand::pop_assignment_for_cores(block_num, 3);
			assert_eq!(assignments.next(), None);
		}
	});
}

#[test]
fn revenue_information_fetching_works() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);
		// Mock assigner sets max revenue history to 10.
		run_to_block(10, |n| if n == 10 { Some(Default::default()) } else { None });
		let revenue = OnDemand::claim_revenue_until(10);

		// No revenue should be recorded.
		assert_eq!(revenue, 0);

		// Place one order
		place_order_run_to_blocknumber(para_a, Some(11));
		let revenue = OnDemand::get_revenue();
		let amt = OnDemand::claim_revenue_until(11);

		// Revenue until the current block is still zero as "until" is non-inclusive
		assert_eq!(amt, 0);

		let amt = OnDemand::claim_revenue_until(12);

		// Revenue for a single order should be recorded and shouldn't have been pruned by the
		// previous call
		assert_eq!(amt, revenue[0]);

		run_to_block(12, |n| if n == 12 { Some(Default::default()) } else { None });
		let revenue = OnDemand::claim_revenue_until(13);

		// No revenue should be recorded.
		assert_eq!(revenue, 0);

		// Place many orders
		place_order(para_a);
		place_order(para_a);

		run_to_block(13, |n| if n == 13 { Some(Default::default()) } else { None });

		place_order(para_a);

		run_to_block(14, |n| if n == 14 { Some(Default::default()) } else { None });

		let revenue = OnDemand::claim_revenue_until(15);

		// All 3 orders should be accounted for.
		assert_eq!(revenue, 30_000);

		// Place one order
		place_order_run_to_blocknumber(para_a, Some(16));

		let revenue = OnDemand::claim_revenue_until(15);

		// Order is not in range of  the revenue_until call
		assert_eq!(revenue, 0);

		run_to_block(20, |n| if n == 20 { Some(Default::default()) } else { None });
		let revenue = OnDemand::claim_revenue_until(21);
		assert_eq!(revenue, 10_000);

		// Make sure overdue revenue is accumulated
		for i in 21..=35 {
			run_to_block(i, |n| if n % 10 == 0 { Some(Default::default()) } else { None });
			place_order(para_a);
		}
		let revenue = OnDemand::claim_revenue_until(36);
		assert_eq!(revenue, 150_000);
	});
}

#[test]
fn pot_account_is_immortal() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(111);
		let pot = OnDemand::account_id();
		assert!(!System::account_exists(&pot));
		schedule_blank_para(para_a, ParaKind::Parathread);
		// Mock assigner sets max revenue history to 10.

		run_to_block(10, |n| if n == 10 { Some(Default::default()) } else { None });
		place_order_run_to_blocknumber(para_a, Some(12));
		let purchase_revenue = Balances::free_balance(&pot);
		assert!(purchase_revenue > 0);

		run_to_block(15, |_| None);
		let _imb = <Test as on_demand::Config>::Currency::withdraw(
			&pot,
			purchase_revenue,
			WithdrawReasons::FEE,
			ExistenceRequirement::AllowDeath,
		);
		assert_eq!(Balances::free_balance(&pot), 0);
		assert!(System::account_exists(&pot));
		assert_eq!(System::providers(&pot), 1);

		// One more cycle to make sure providers are not increased on every transition from zero
		run_to_block(20, |n| if n == 20 { Some(Default::default()) } else { None });
		place_order_run_to_blocknumber(para_a, Some(22));
		let purchase_revenue = Balances::free_balance(&pot);
		assert!(purchase_revenue > 0);

		run_to_block(25, |_| None);
		let _imb = <Test as on_demand::Config>::Currency::withdraw(
			&pot,
			purchase_revenue,
			WithdrawReasons::FEE,
			ExistenceRequirement::AllowDeath,
		);
		assert_eq!(Balances::free_balance(&pot), 0);
		assert!(System::account_exists(&pot));
		assert_eq!(System::providers(&pot), 1);
	});
}

#[test]
fn queue_on_demand_batch_works() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(110);
		let para_b = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);
		schedule_blank_para(para_b, ParaKind::Parathread);

		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		assert_ok!(Coretime::queue_on_demand_batch(
			broker_origin(),
			vec![(para_a, block_num), (para_b, block_num)]
		));

		// Async backing: orders only become ready two blocks after they were placed.
		assert_eq!(OnDemand::pop_assignment_for_cores(block_num + 1, 2).next(), None);

		let mut assignments = OnDemand::pop_assignment_for_cores(block_num + 2, 2);
		assert_eq!(assignments.next(), Some(para_a));
		assert_eq!(assignments.next(), Some(para_b));
		assert_eq!(assignments.next(), None);
	});
}

#[test]
fn queue_on_demand_batch_is_free() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);

		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		// Batched orders have already been paid for on the coretime chain, so nothing is
		// collected for them here.
		assert_ok!(Coretime::queue_on_demand_batch(broker_origin(), vec![(para_a, block_num)]));
		assert_eq!(OnDemand::pop_assignment_for_cores(block_num + 2, 1).next(), Some(para_a));

		assert_eq!(Balances::free_balance(&OnDemand::account_id()), 0);
		assert_eq!(OnDemand::claim_revenue_until(block_num + 1), 0);
	});
}

#[test]
fn queue_on_demand_batch_with_empty_batch_is_a_noop() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		assert_ok!(Coretime::queue_on_demand_batch(broker_origin(), vec![]));
		assert_eq!(OnDemand::pop_assignment_for_cores(block_num + 2, 2).next(), None);
	});
}

#[test]
fn queue_on_demand_batch_respects_ordered_at() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(110);
		let para_b = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);
		schedule_blank_para(para_b, ParaKind::Parathread);

		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		// The coretime chain tells us when an order was placed, which is not necessarily the
		// current block and the batch does not have to be sorted.
		assert_ok!(Coretime::queue_on_demand_batch(
			broker_origin(),
			vec![(para_b, block_num + 1), (para_a, block_num)]
		));

		// Only the earlier order is ready here ...
		let mut assignments = OnDemand::pop_assignment_for_cores(block_num + 2, 2);
		assert_eq!(assignments.next(), Some(para_a));
		assert_eq!(assignments.next(), None);

		// ... the later one becomes ready a block afterwards.
		let mut assignments = OnDemand::pop_assignment_for_cores(block_num + 3, 2);
		assert_eq!(assignments.next(), Some(para_b));
		assert_eq!(assignments.next(), None);
	});
}

#[test]
fn queue_on_demand_batch_keeps_duplicate_orders() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);

		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		assert_ok!(Coretime::queue_on_demand_batch(
			broker_origin(),
			vec![(para_a, block_num), (para_a, block_num)]
		));

		// Affinity prohibits serving both orders in the same block ...
		let mut assignments = OnDemand::pop_assignment_for_cores(block_num + 2, 2);
		assert_eq!(assignments.next(), Some(para_a));
		assert_eq!(assignments.next(), None);

		// ... but the second one is not dropped.
		let mut assignments = OnDemand::pop_assignment_for_cores(block_num + 3, 2);
		assert_eq!(assignments.next(), Some(para_a));
		assert_eq!(assignments.next(), None);
	});
}

#[test]
fn queue_on_demand_batch_beyond_capacity_fails() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		// Fill the queue up to its capacity.
		let batch: Vec<_> = (0..ON_DEMAND_MAX_QUEUE_MAX_SIZE)
			.map(|i| (ParaId::from(i), block_num))
			.collect();
		assert_ok!(Coretime::queue_on_demand_batch(broker_origin(), batch));

		// Any further order does not fit anymore. The call will still succeed, but an event
		// communicating that the queue was full will be emitted.
		assert_ok!(Coretime::queue_on_demand_batch(
			broker_origin(),
			vec![(ParaId::from(111), block_num)]
		),);
		assert_last_event(RuntimeEvent::OnDemand(Event::UnexpectedQueueFull { dropped: 1 }));
	});
}

#[test]
fn queue_on_demand_batch_rejects_non_broker_origins() {
	new_test_ext(GenesisConfigBuilder::default().build()).execute_with(|| {
		let para_a = ParaId::from(111);
		schedule_blank_para(para_a, ParaKind::Parathread);

		let block_num = 11;
		run_to_block(block_num, |n| if n == 11 { Some(Default::default()) } else { None });

		let batch = vec![(para_a, block_num)];

		// A parachain other than the broker is not allowed to queue orders ...
		let other_para = ParaId::from(BrokerId::get() + 1);
		assert_noop!(
			Coretime::queue_on_demand_batch(
				RuntimeOrigin::from(ParachainOrigin::Parachain(other_para)),
				batch.clone()
			),
			crate::coretime::Error::<Test>::NotBroker
		);

		// ... and neither are signed accounts.
		assert_noop!(
			Coretime::queue_on_demand_batch(RuntimeOrigin::signed(1), batch.clone()),
			BadOrigin
		);
		assert_noop!(Coretime::queue_on_demand_batch(RuntimeOrigin::none(), batch), BadOrigin);

		assert_eq!(OnDemand::pop_assignment_for_cores(block_num + 2, 1).next(), None);
	});
}
