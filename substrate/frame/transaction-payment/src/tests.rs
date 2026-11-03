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
use crate as pallet_transaction_payment;

use codec::Encode;

use sp_runtime::{
	generic::UncheckedExtrinsic,
	traits::{DispatchTransaction, One},
	transaction_validity::{InvalidTransaction, TransactionSource::External},
	BuildStorage,
};

use frame_support::{
	assert_ok,
	dispatch::{DispatchClass, DispatchInfo, GetDispatchInfo, PostDispatchInfo},
	traits::{Currency, OriginTrait},
	weights::Weight,
};
use frame_system as system;
use mock::*;
use pallet_balances::Call as BalancesCall;

pub struct ExtBuilder {
	balance_factor: u64,
	base_weight: Weight,
	byte_fee: u64,
	weight_to_fee: u64,
	initial_multiplier: Option<Multiplier>,
}

impl Default for ExtBuilder {
	fn default() -> Self {
		Self {
			balance_factor: 1,
			base_weight: Weight::zero(),
			byte_fee: 1,
			weight_to_fee: 1,
			initial_multiplier: None,
		}
	}
}

impl ExtBuilder {
	pub fn base_weight(mut self, base_weight: Weight) -> Self {
		self.base_weight = base_weight;
		self
	}
	pub fn byte_fee(mut self, byte_fee: u64) -> Self {
		self.byte_fee = byte_fee;
		self
	}
	pub fn weight_fee(mut self, weight_to_fee: u64) -> Self {
		self.weight_to_fee = weight_to_fee;
		self
	}
	pub fn balance_factor(mut self, factor: u64) -> Self {
		self.balance_factor = factor;
		self
	}
	pub fn with_initial_multiplier(mut self, multiplier: Multiplier) -> Self {
		self.initial_multiplier = Some(multiplier);
		self
	}
	fn set_constants(&self) {
		ExtrinsicBaseWeight::mutate(|v| *v = self.base_weight);
		TRANSACTION_BYTE_FEE.with(|v| *v.borrow_mut() = self.byte_fee);
		WEIGHT_TO_FEE.with(|v| *v.borrow_mut() = self.weight_to_fee);
	}
	pub fn build(self) -> sp_io::TestExternalities {
		self.set_constants();
		let mut t = frame_system::GenesisConfig::<Runtime>::default().build_storage().unwrap();
		pallet_balances::GenesisConfig::<Runtime> {
			balances: if self.balance_factor > 0 {
				vec![
					(1, 10 * self.balance_factor),
					(2, 20 * self.balance_factor),
					(3, 30 * self.balance_factor),
					(4, 40 * self.balance_factor),
					(5, 50 * self.balance_factor),
					(6, 60 * self.balance_factor),
				]
			} else {
				vec![]
			},
			..Default::default()
		}
		.assimilate_storage(&mut t)
		.unwrap();

		if let Some(multiplier) = self.initial_multiplier {
			pallet::GenesisConfig::<Runtime> { multiplier, ..Default::default() }
				.assimilate_storage(&mut t)
				.unwrap();
		}

		t.into()
	}
}

/// create a transaction info struct from weight. Handy to avoid building the whole struct.
pub fn info_from_weight(w: Weight) -> DispatchInfo {
	// pays_fee: Pays::Yes -- class: DispatchClass::Normal
	DispatchInfo { call_weight: w, ..Default::default() }
}

fn post_info_from_weight(w: Weight) -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: Some(w), pays_fee: Default::default() }
}

fn post_info_from_pays(p: Pays) -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: None, pays_fee: p }
}

fn default_post_info() -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: None, pays_fee: Default::default() }
}

type Ext = ChargeTransactionPayment<Runtime>;

#[test]
fn transaction_extension_transaction_payment_work() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(5, 0))
		.build()
		.execute_with(|| {
			let mut info = info_from_weight(Weight::from_parts(5, 0));
			let ext = Ext::from(0);
			let ext_weight = ext.weight(CALL);
			info.extension_weight = ext_weight;
			ext.test_run(Some(1).into(), CALL, &info, 10, 0, |_| {
				assert_eq!(Balances::free_balance(1), 100 - 5 - 5 - 10 - 10);
				Ok(default_post_info())
			})
			.unwrap()
			.unwrap();
			assert_eq!(Balances::free_balance(1), 100 - 5 - 5 - 10 - 10);
			assert_eq!(FeeUnbalancedAmount::get(), 5 + 5 + 10 + 10);
			assert_eq!(TipUnbalancedAmount::get(), 0);

			FeeUnbalancedAmount::mutate(|a| *a = 0);

			let mut info = info_from_weight(Weight::from_parts(100, 0));
			info.extension_weight = ext_weight;
			Ext::from(5 /* tipped */)
				.test_run(Some(2).into(), CALL, &info, 10, 0, |_| {
					assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 10 - 5);
					Ok(post_info_from_weight(Weight::from_parts(50, 0)))
				})
				.unwrap()
				.unwrap();
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 50 - 10 - 5);
			assert_eq!(FeeUnbalancedAmount::get(), 5 + 10 + 50 + 10);
			assert_eq!(TipUnbalancedAmount::get(), 5);
		});
}

#[test]
fn transaction_extension_transaction_payment_multiplied_refund_works() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(5, 0))
		.build()
		.execute_with(|| {
			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(3, 2));

			let len = 10;
			let origin = Some(2).into();
			let mut info = info_from_weight(Weight::from_parts(100, 0));
			let ext = Ext::from(5 /* tipped */);
			let ext_weight = ext.weight(CALL);
			info.extension_weight = ext_weight;
			ext.test_run(origin, CALL, &info, len, 0, |_| {
				// 5 base fee, 10 byte fee, 3/2 * (100 call weight fee + 10 ext weight fee), 5
				// tip
				assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 165 - 5);
				Ok(post_info_from_weight(Weight::from_parts(50, 0)))
			})
			.unwrap()
			.unwrap();

			// 75 (3/2 of the returned 50 units of call weight, 0 returned of ext weight) is
			// refunded
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - (165 - 75) - 5);
		});
}

#[test]
fn transaction_extension_transaction_payment_is_bounded() {
	ExtBuilder::default().balance_factor(1000).byte_fee(0).build().execute_with(|| {
		// maximum weight possible
		let info = info_from_weight(Weight::MAX);
		assert_ok!(Ext::from(0).validate_and_prepare(Some(1).into(), CALL, &info, 10, 0));
		// fee will be proportional to what is the actual maximum weight in the runtime.
		assert_eq!(
			Balances::free_balance(&1),
			(10000 - <Runtime as frame_system::Config>::BlockWeights::get().max_block.ref_time())
				as u64
		);
	});
}

#[test]
fn transaction_extension_allows_free_transactions() {
	ExtBuilder::default()
		.base_weight(Weight::from_parts(100, 0))
		.balance_factor(0)
		.build()
		.execute_with(|| {
			// 1 ain't have a penny.
			assert_eq!(Balances::free_balance(1), 0);

			let len = 100;

			// This is a completely free (and thus wholly insecure/DoS-ridden) transaction.
			let op_tx = DispatchInfo {
				call_weight: Weight::from_parts(0, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::No,
			};
			assert_ok!(Ext::from(0).validate_only(Some(1).into(), CALL, &op_tx, len, External, 0));

			// like a InsecureFreeNormal
			let free_tx = DispatchInfo {
				call_weight: Weight::from_parts(0, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Normal,
				pays_fee: Pays::Yes,
			};
			assert_eq!(
				Ext::from(0)
					.validate_only(Some(1).into(), CALL, &free_tx, len, External, 0)
					.unwrap_err(),
				TransactionValidityError::Invalid(InvalidTransaction::Payment),
			);
		});
}

#[test]
fn transaction_ext_length_fee_is_also_updated_per_congestion() {
	ExtBuilder::default()
		.base_weight(Weight::from_parts(5, 0))
		.balance_factor(10)
		.build()
		.execute_with(|| {
			// all fees should be x1.5
			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(3, 2));
			let len = 10;
			let info = info_from_weight(Weight::from_parts(3, 0));
			assert_ok!(Ext::from(10).validate_and_prepare(Some(1).into(), CALL, &info, len, 0));
			assert_eq!(
				Balances::free_balance(1),
				100 // original
			- 10 // tip
			- 5 // base
			- 10 // len
			- (3 * 3 / 2) // adjusted weight
			);
		})
}

#[test]
fn query_info_and_fee_details_works() {
	let call = RuntimeCall::Balances(BalancesCall::transfer_allow_death { dest: 2, value: 69 });
	let origin = 111111;
	let extra = ();
	let xt = UncheckedExtrinsic::<_, _, _, _>::new_signed(call.clone(), origin, (), extra);
	let info = xt.get_dispatch_info();
	let ext = xt.encode();
	let len = ext.len() as u32;

	let unsigned_xt = UncheckedExtrinsic::<u64, _, (), ()>::new_bare(call);
	let unsigned_xt_info = unsigned_xt.get_dispatch_info();

	ExtBuilder::default()
		.base_weight(Weight::from_parts(5, 0))
		.weight_fee(2)
		.build()
		.execute_with(|| {
			// all fees should be x1.5
			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(3, 2));

			assert_eq!(
				TransactionPayment::query_info(xt.clone(), len),
				RuntimeDispatchInfo {
					weight: info.total_weight(),
					class: info.class,
					partial_fee: 5 * 2 /* base * weight_fee */
					+ len as u64  /* len * 1 */
					+ info.total_weight().min(BlockWeights::get().max_block).ref_time() as u64 * 2 * 3 / 2 /* weight */
				},
			);

			assert_eq!(
				TransactionPayment::query_info(unsigned_xt.clone(), len),
				RuntimeDispatchInfo {
					weight: unsigned_xt_info.call_weight,
					class: unsigned_xt_info.class,
					partial_fee: 0,
				},
			);

			assert_eq!(
				TransactionPayment::query_fee_details(xt, len),
				FeeDetails {
					inclusion_fee: Some(InclusionFee {
						base_fee: 5 * 2,
						len_fee: len as u64,
						adjusted_weight_fee: info
							.total_weight()
							.min(BlockWeights::get().max_block)
							.ref_time() as u64 * 2 * 3 / 2
					}),
					tip: 0,
				},
			);

			assert_eq!(
				TransactionPayment::query_fee_details(unsigned_xt, len),
				FeeDetails { inclusion_fee: None, tip: 0 },
			);
		});
}

#[test]
fn query_call_info_and_fee_details_works() {
	let call = RuntimeCall::Balances(BalancesCall::transfer_allow_death { dest: 2, value: 69 });
	let info = call.get_dispatch_info();
	let encoded_call = call.encode();
	let len = encoded_call.len() as u32;

	ExtBuilder::default()
		.base_weight(Weight::from_parts(5, 0))
		.weight_fee(2)
		.build()
		.execute_with(|| {
			// all fees should be x1.5
			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(3, 2));

			assert_eq!(
				TransactionPayment::query_call_info(call.clone(), len),
				RuntimeDispatchInfo {
					weight: info.total_weight(),
					class: info.class,
					partial_fee: 5 * 2 /* base * weight_fee */
					+ len as u64  /* len * 1 */
					+ info.total_weight().min(BlockWeights::get().max_block).ref_time() as u64 * 2 * 3 / 2 /* weight */
				},
			);

			assert_eq!(
				TransactionPayment::query_call_fee_details(call, len),
				FeeDetails {
					inclusion_fee: Some(InclusionFee {
						base_fee: 5 * 2,     /* base * weight_fee */
						len_fee: len as u64, /* len * 1 */
						adjusted_weight_fee: info
							.total_weight()
							.min(BlockWeights::get().max_block)
							.ref_time() as u64 * 2 * 3 / 2  /* weight * weight_fee * multipler */
					}),
					tip: 0,
				},
			);
		});
}

#[test]
fn compute_fee_works_without_multiplier() {
	ExtBuilder::default()
		.base_weight(Weight::from_parts(100, 0))
		.byte_fee(10)
		.balance_factor(0)
		.build()
		.execute_with(|| {
			// Next fee multiplier is zero
			assert_eq!(NextFeeMultiplier::<Runtime>::get(), Multiplier::one());

			// Tip only, no fees works
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(0, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::No,
			};
			assert_eq!(Pallet::<Runtime>::compute_fee(0, &dispatch_info, 10), 10);
			// No tip, only base fee works
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(0, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			assert_eq!(Pallet::<Runtime>::compute_fee(0, &dispatch_info, 0), 100);
			// Tip + base fee works
			assert_eq!(Pallet::<Runtime>::compute_fee(0, &dispatch_info, 69), 169);
			// Len (byte fee) + base fee works
			assert_eq!(Pallet::<Runtime>::compute_fee(42, &dispatch_info, 0), 520);
			// Weight fee + base fee works
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(1000, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			assert_eq!(Pallet::<Runtime>::compute_fee(0, &dispatch_info, 0), 1100);
		});
}

#[test]
fn compute_fee_works_with_multiplier() {
	ExtBuilder::default()
		.base_weight(Weight::from_parts(100, 0))
		.byte_fee(10)
		.balance_factor(0)
		.build()
		.execute_with(|| {
			// Add a next fee multiplier. Fees will be x3/2.
			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(3, 2));
			// Base fee is unaffected by multiplier
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(0, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			assert_eq!(Pallet::<Runtime>::compute_fee(0, &dispatch_info, 0), 100);

			// Everything works together :)
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(123, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			// 123 weight, 456 length, 100 base
			assert_eq!(
				Pallet::<Runtime>::compute_fee(456, &dispatch_info, 789),
				100 + (3 * 123 / 2) + 4560 + 789,
			);
		});
}

#[test]
fn compute_fee_works_with_negative_multiplier() {
	ExtBuilder::default()
		.base_weight(Weight::from_parts(100, 0))
		.byte_fee(10)
		.balance_factor(0)
		.build()
		.execute_with(|| {
			// Add a next fee multiplier. All fees will be x1/2.
			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(1, 2));

			// Base fee is unaffected by multiplier.
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(0, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			assert_eq!(Pallet::<Runtime>::compute_fee(0, &dispatch_info, 0), 100);

			// Everything works together.
			let dispatch_info = DispatchInfo {
				call_weight: Weight::from_parts(123, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			// 123 weight, 456 length, 100 base
			assert_eq!(
				Pallet::<Runtime>::compute_fee(456, &dispatch_info, 789),
				100 + (123 / 2) + 4560 + 789,
			);
		});
}

#[test]
fn compute_fee_does_not_overflow() {
	ExtBuilder::default()
		.base_weight(Weight::from_parts(100, 0))
		.byte_fee(10)
		.balance_factor(0)
		.build()
		.execute_with(|| {
			// Overflow is handled
			let dispatch_info = DispatchInfo {
				call_weight: Weight::MAX,
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			assert_eq!(
				Pallet::<Runtime>::compute_fee(u32::MAX, &dispatch_info, u64::MAX),
				u64::MAX
			);
		});
}

#[test]
fn refund_does_not_recreate_account() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(5, 0))
		.build()
		.execute_with(|| {
			// So events are emitted
			System::set_block_number(10);
			let info = info_from_weight(Weight::from_parts(100, 0));
			Ext::from(5 /* tipped */)
				.test_run(Some(2).into(), CALL, &info, 10, 0, |origin| {
					assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);

					// kill the account between pre and post dispatch
					assert_ok!(Balances::transfer_allow_death(
						origin,
						3,
						Balances::free_balance(2)
					));
					assert_eq!(Balances::free_balance(2), 0);

					Ok(post_info_from_weight(Weight::from_parts(50, 0)))
				})
				.unwrap()
				.unwrap();
			assert_eq!(Balances::free_balance(2), 0);
			// Transfer Event
			System::assert_has_event(RuntimeEvent::Balances(pallet_balances::Event::Transfer {
				from: 2,
				to: 3,
				amount: 80,
			}));
			// Killed Event
			System::assert_has_event(RuntimeEvent::System(system::Event::KilledAccount {
				account: 2,
			}));
		});
}

#[test]
fn actual_weight_higher_than_max_refunds_nothing() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(5, 0))
		.build()
		.execute_with(|| {
			let info = info_from_weight(Weight::from_parts(100, 0));
			Ext::from(5 /* tipped */)
				.test_run(Some(2).into(), CALL, &info, 10, 0, |_| {
					assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);
					Ok(post_info_from_weight(Weight::from_parts(101, 0)))
				})
				.unwrap()
				.unwrap();
			assert_eq!(Balances::free_balance(2), 200 - 5 - 10 - 100 - 5);
		});
}

#[test]
fn zero_transfer_on_free_transaction() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(5, 0))
		.build()
		.execute_with(|| {
			// So events are emitted
			System::set_block_number(10);
			let info = DispatchInfo {
				call_weight: Weight::from_parts(100, 0),
				extension_weight: Weight::zero(),
				pays_fee: Pays::No,
				class: DispatchClass::Normal,
			};
			let user = 69;
			Ext::from(0)
				.test_run(Some(user).into(), CALL, &info, 10, 0, |_| {
					assert_eq!(Balances::total_balance(&user), 0);
					Ok(default_post_info())
				})
				.unwrap()
				.unwrap();
			assert_eq!(Balances::total_balance(&user), 0);
			// TransactionFeePaid Event
			System::assert_has_event(RuntimeEvent::TransactionPayment(
				pallet_transaction_payment::Event::TransactionFeePaid {
					who: user,
					actual_fee: 0,
					tip: 0,
				},
			));
		});
}

#[test]
fn refund_consistent_with_actual_weight() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(7, 0))
		.build()
		.execute_with(|| {
			let mut info = info_from_weight(Weight::from_parts(100, 0));
			let tip = 5;
			let ext = Ext::from(tip);
			let ext_weight = ext.weight(CALL);
			info.extension_weight = ext_weight;
			let mut post_info = post_info_from_weight(Weight::from_parts(33, 0));
			let prev_balance = Balances::free_balance(2);
			let len = 10;

			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(5, 4));

			let actual_post_info = ext
				.test_run(Some(2).into(), CALL, &info, len, 0, |_| Ok(post_info))
				.unwrap()
				.unwrap();
			post_info
				.actual_weight
				.as_mut()
				.map(|w| w.saturating_accrue(Ext::from(tip).weight(CALL)));
			assert_eq!(post_info, actual_post_info);

			let refund_based_fee = prev_balance - Balances::free_balance(2);
			let actual_fee =
				Pallet::<Runtime>::compute_actual_fee(len as u32, &info, &actual_post_info, tip);

			// 33 call weight, 10 ext weight, 10 length, 7 base, 5 tip
			assert_eq!(actual_fee, 7 + 10 + ((33 + 10) * 5 / 4) + 5);
			assert_eq!(refund_based_fee, actual_fee);
		});
}

#[test]
fn should_alter_operational_priority() {
	let tip = 5;
	let len = 10;

	ExtBuilder::default().balance_factor(100).build().execute_with(|| {
		let normal = DispatchInfo {
			call_weight: Weight::from_parts(100, 0),
			extension_weight: Weight::zero(),
			class: DispatchClass::Normal,
			pays_fee: Pays::Yes,
		};

		let ext = Ext::from(tip);
		let priority = ext
			.validate_only(Some(2).into(), CALL, &normal, len, External, 0)
			.unwrap()
			.0
			.priority;
		assert_eq!(priority, 60);

		let ext = Ext::from(2 * tip);
		let priority = ext
			.validate_only(Some(2).into(), CALL, &normal, len, External, 0)
			.unwrap()
			.0
			.priority;
		assert_eq!(priority, 110);
	});

	ExtBuilder::default().balance_factor(100).build().execute_with(|| {
		let op = DispatchInfo {
			call_weight: Weight::from_parts(100, 0),
			extension_weight: Weight::zero(),
			class: DispatchClass::Operational,
			pays_fee: Pays::Yes,
		};

		let ext = Ext::from(tip);
		let priority = ext
			.validate_only(Some(2).into(), CALL, &op, len, External, 0)
			.unwrap()
			.0
			.priority;
		assert_eq!(priority, 5810);

		let ext = Ext::from(2 * tip);
		let priority = ext
			.validate_only(Some(2).into(), CALL, &op, len, External, 0)
			.unwrap()
			.0
			.priority;
		assert_eq!(priority, 6110);
	});
}

#[test]
fn no_tip_has_some_priority() {
	let tip = 0;
	let len = 10;

	ExtBuilder::default().balance_factor(100).build().execute_with(|| {
		let normal = DispatchInfo {
			call_weight: Weight::from_parts(100, 0),
			extension_weight: Weight::zero(),
			class: DispatchClass::Normal,
			pays_fee: Pays::Yes,
		};
		let ext = Ext::from(tip);
		let priority = ext
			.validate_only(Some(2).into(), CALL, &normal, len, External, 0)
			.unwrap()
			.0
			.priority;
		assert_eq!(priority, 10);
	});

	ExtBuilder::default().balance_factor(100).build().execute_with(|| {
		let op = DispatchInfo {
			call_weight: Weight::from_parts(100, 0),
			extension_weight: Weight::zero(),
			class: DispatchClass::Operational,
			pays_fee: Pays::Yes,
		};
		let ext = Ext::from(tip);
		let priority = ext
			.validate_only(Some(2).into(), CALL, &op, len, External, 0)
			.unwrap()
			.0
			.priority;
		assert_eq!(priority, 5510);
	});
}

#[test]
fn higher_tip_have_higher_priority() {
	let get_priorities = |tip: u64| {
		let mut pri1 = 0;
		let mut pri2 = 0;
		let len = 10;
		ExtBuilder::default().balance_factor(100).build().execute_with(|| {
			let normal = DispatchInfo {
				call_weight: Weight::from_parts(100, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Normal,
				pays_fee: Pays::Yes,
			};
			let ext = Ext::from(tip);

			pri1 = ext
				.validate_only(Some(2).into(), CALL, &normal, len, External, 0)
				.unwrap()
				.0
				.priority;
		});

		ExtBuilder::default().balance_factor(100).build().execute_with(|| {
			let op = DispatchInfo {
				call_weight: Weight::from_parts(100, 0),
				extension_weight: Weight::zero(),
				class: DispatchClass::Operational,
				pays_fee: Pays::Yes,
			};
			let ext = Ext::from(tip);
			pri2 = ext
				.validate_only(Some(2).into(), CALL, &op, len, External, 0)
				.unwrap()
				.0
				.priority;
		});

		(pri1, pri2)
	};

	let mut prev_priorities = get_priorities(0);

	for tip in 1..3 {
		let priorities = get_priorities(tip);
		assert!(prev_priorities.0 < priorities.0);
		assert!(prev_priorities.1 < priorities.1);
		prev_priorities = priorities;
	}
}

#[test]
fn post_info_can_change_pays_fee() {
	ExtBuilder::default()
		.balance_factor(10)
		.base_weight(Weight::from_parts(7, 0))
		.build()
		.execute_with(|| {
			let info = info_from_weight(Weight::from_parts(100, 0));
			let post_info = post_info_from_pays(Pays::No);
			let prev_balance = Balances::free_balance(2);
			let len = 10;
			let tip = 5;

			NextFeeMultiplier::<Runtime>::put(Multiplier::saturating_from_rational(5, 4));

			let post_info = ChargeTransactionPayment::<Runtime>::from(tip)
				.test_run(Some(2).into(), CALL, &info, len, 0, |_| Ok(post_info))
				.unwrap()
				.unwrap();

			let refund_based_fee = prev_balance - Balances::free_balance(2);
			let actual_fee =
				Pallet::<Runtime>::compute_actual_fee(len as u32, &info, &post_info, tip);

			// Only 5 tip is paid
			assert_eq!(actual_fee, 5);
			assert_eq!(refund_based_fee, actual_fee);
		});
}

#[test]
fn genesis_config_works() {
	ExtBuilder::default()
		.with_initial_multiplier(Multiplier::from_u32(100))
		.build()
		.execute_with(|| {
			assert_eq!(
				NextFeeMultiplier::<Runtime>::get(),
				Multiplier::saturating_from_integer(100)
			);
		});
}

#[test]
fn genesis_default_works() {
	ExtBuilder::default().build().execute_with(|| {
		assert_eq!(NextFeeMultiplier::<Runtime>::get(), Multiplier::saturating_from_integer(1));
	});
}

#[test]
fn no_fee_and_no_weight_for_other_origins() {
	ExtBuilder::default().build().execute_with(|| {
		let ext = Ext::from(0);

		let mut info = CALL.get_dispatch_info();
		info.extension_weight = ext.weight(CALL);

		// Ensure we test the refund.
		assert!(info.extension_weight != Weight::zero());

		let len = CALL.encoded_size();

		let origin = frame_system::RawOrigin::Root.into();
		let (pre, origin) = ext.validate_and_prepare(origin, CALL, &info, len, 0).unwrap();

		assert!(origin.as_system_ref().unwrap().is_root());

		let pd_res = Ok(());
		let mut post_info = frame_support::dispatch::PostDispatchInfo {
			actual_weight: Some(info.total_weight()),
			pays_fee: Default::default(),
		};

		<Ext as TransactionExtension<RuntimeCall>>::post_dispatch(
			pre,
			&info,
			&mut post_info,
			len,
			&pd_res,
		)
		.unwrap();

		assert_eq!(post_info.actual_weight, Some(info.call_weight));
	})
}

#[test]
fn fungible_adapter_no_zero_refund_action() {
	type FungibleAdapterT = payment::FungibleAdapter<Balances, DealWithFees>;

	ExtBuilder::default().balance_factor(10).build().execute_with(|| {
		System::set_block_number(10);

		let dummy_acc = 1;
		let (actual_fee, no_tip) = (10, 0);
		let already_paid = <FungibleAdapterT as OnChargeTransaction<Runtime>>::withdraw_fee(
			&dummy_acc,
			CALL,
			&CALL.get_dispatch_info(),
			actual_fee,
			no_tip,
		).expect("Account must have enough funds.");

		// Correction action with no expected side effect.
		assert!(<FungibleAdapterT as OnChargeTransaction<Runtime>>::correct_and_deposit_fee(
			&dummy_acc,
			&CALL.get_dispatch_info(),
			&default_post_info(),
			actual_fee,
			no_tip,
			already_paid,
		).is_ok());

		// Ensure no zero amount deposit event is emitted.
		let events = System::events();
		assert!(!events
			.iter()
			.any(|record| matches!(record.event, RuntimeEvent::Balances(pallet_balances::Event::Deposit { amount, .. }) if amount.is_zero())),
    		"No zero amount deposit amount event should be emitted.",
		);
	});
}
