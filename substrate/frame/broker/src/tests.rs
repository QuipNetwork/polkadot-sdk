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

#![cfg(test)]

use crate::{mock::*, *};
use frame_support::{
	assert_err, assert_noop, assert_ok,
	traits::nonfungible::{Inspect as NftInspect, Mutate, Transfer},
	BoundedVec,
};
use frame_system::RawOrigin::Root;
use pretty_assertions::assert_eq;
use sp_runtime::{
	traits::{BadOrigin, Get},
	DispatchError, Perbill, TokenError,
};
use CoreAssignment::*;
use CoretimeTraceItem::*;
use Finality::*;

#[test]
fn basic_initialize_works() {
	TestExt::new().execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		assert_eq!(CoretimeTrace::get(), vec![]);
		assert_eq!(Broker::current_timeslice(), 0);
	});
}

#[test]
fn drop_region_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, Some(1), 1001, Provisional));
		advance_to(11);
		assert_noop!(Broker::do_drop_region(region), Error::<Test>::StillValid);
		advance_to(12);
		// assignment worked.
		let just_1001 = vec![(Task(1001), 57600)];
		let just_pool = vec![(Pool, 57600)];
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(6, AssignCore { core: 0, begin: 8, assignment: just_1001, end_hint: None }),
				(12, AssignCore { core: 0, begin: 14, assignment: just_pool, end_hint: None }),
			]
		);
		// `region` still exists as it was never finalized.
		assert_eq!(Regions::<Test>::iter().count(), 1);
		assert_ok!(Broker::do_drop_region(region));
		assert_eq!(Regions::<Test>::iter().count(), 0);
		assert_noop!(Broker::do_drop_region(region), Error::<Test>::UnknownRegion);
	});
}

#[test]
fn drop_renewal_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, Some(1), 1001, Final));
		advance_to(11);
		let e = Error::<Test>::StillValid;
		assert_noop!(Broker::do_drop_renewal(region.core, region.begin + 3), e);
		advance_to(12);
		assert_eq!(PotentialRenewals::<Test>::iter().count(), 1);
		assert_ok!(Broker::do_drop_renewal(region.core, region.begin + 3));
		assert_eq!(PotentialRenewals::<Test>::iter().count(), 0);
		let e = Error::<Test>::UnknownRenewal;
		assert_noop!(Broker::do_drop_renewal(region.core, region.begin + 3), e);
	});
}

#[test]
fn drop_contribution_works() {
	TestExt::new().contribution_timeout(3).endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		// Place region in pool. Active in pool timeslices 4, 5, 6 = rcblocks 8, 10, 12; we
		// expect the contribution record to timeout 3 timeslices following 7 = 14
		//
		// Due to the contribution_timeout being configured for 3 timeslices, the contribution
		// can only be discarded at timeslice 10, i.e. rcblock 20.
		assert_ok!(Broker::do_pool(region, Some(1), 1, Final));
		assert_eq!(InstaPoolContribution::<Test>::iter().count(), 1);
		advance_to(19);
		assert_noop!(Broker::do_drop_contribution(region), Error::<Test>::StillValid);
		advance_to(20);
		assert_ok!(Broker::do_drop_contribution(region));
		assert_eq!(InstaPoolContribution::<Test>::iter().count(), 0);
		assert_noop!(Broker::do_drop_contribution(region), Error::<Test>::UnknownContribution);
	});
}

#[test]
fn drop_history_works() {
	TestExt::new()
		.contribution_timeout(4)
		.endow(1, 1000)
		.endow(2, 50)
		.execute_with(|| {
			assert_ok!(Broker::do_start_sales(100, 1));
			advance_to(2);
			let mut region = Broker::do_purchase(1, u64::max_value()).unwrap();
			// Place region in pool. Active in pool timeslices 4, 5, 6 = rcblocks 8, 10, 12; we
			// expect to make/receive revenue reports on blocks 10, 12, 14.
			assert_ok!(Broker::do_pool(region, Some(1), 1, Final));
			assert_ok!(Broker::do_purchase_credit(2, 50, 2));
			advance_to(6);
			// In the stable state with no pending payouts, we expect to see 3 items in
			// InstaPoolHistory here since there is a latency of 1 timeslice (for generating the
			// revenue report), the forward notice period (equivalent to another timeslice) and a
			// block between the revenue report being requested and the response being processed.
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 3);
			advance_to(7);
			// One block later, the most recent report will have been processed, so the effective
			// queue drops to 2 items.
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 2);
			advance_to(8);
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 3);
			assert_ok!(TestCoretimeProvider::spend_instantaneous(2, 10));
			advance_to(10);
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 3);
			assert_ok!(TestCoretimeProvider::spend_instantaneous(2, 10));
			advance_to(12);
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 4);
			assert_ok!(TestCoretimeProvider::spend_instantaneous(2, 10));
			advance_to(14);
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 5);
			advance_to(16);
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 6);
			advance_to(17);
			assert_noop!(Broker::do_drop_history(u32::MAX), Error::<Test>::StillValid);
			assert_noop!(Broker::do_drop_history(region.begin), Error::<Test>::StillValid);
			advance_to(18);
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 6);
			// Block 18 is 8 blocks ()= 4 timeslices = contribution timeout) after first region.
			// Its revenue should now be droppable.
			assert_ok!(Broker::do_drop_history(region.begin));
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 5);
			assert_noop!(Broker::do_drop_history(region.begin), Error::<Test>::NoHistory);
			advance_to(19);
			region.begin += 1;
			assert_noop!(Broker::do_drop_history(region.begin), Error::<Test>::StillValid);
			advance_to(20);
			assert_ok!(Broker::do_drop_history(region.begin));
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 4);
			assert_noop!(Broker::do_drop_history(region.begin), Error::<Test>::NoHistory);
			advance_to(21);
			region.begin += 1;
			assert_noop!(Broker::do_drop_history(region.begin), Error::<Test>::StillValid);
			advance_to(22);
			assert_ok!(Broker::do_drop_history(region.begin));
			assert_eq!(InstaPoolHistory::<Test>::iter().count(), 3);
			assert_noop!(Broker::do_drop_history(region.begin), Error::<Test>::NoHistory);
		});
}

#[test]
fn request_core_count_works() {
	TestExt::new().execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 0));
		assert_ok!(Broker::request_core_count(RuntimeOrigin::root(), 1));
		advance_to(12);
		let assignment = vec![(Pool, 57600)];
		assert_eq!(
			CoretimeTrace::get(),
			vec![(12, AssignCore { core: 0, begin: 14, assignment, end_hint: None })],
		);
	});
}

#[test]
fn transfer_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(<Broker as Transfer<_>>::transfer(&region.into(), &2));
		assert_eq!(<Broker as NftInspect<_>>::owner(&region.into()), Some(2));
		assert_noop!(Broker::do_assign(region, Some(1), 1001, Final), Error::<Test>::NotOwner);
		assert_ok!(Broker::do_assign(region, Some(2), 1002, Final));
	});
}

#[test]
fn mutate_operations_work() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		let region_id = RegionId { begin: 0, core: 0, mask: CoreMask::complete() };
		assert_noop!(
			<Broker as Mutate<_>>::mint_into(&region_id.into(), &2),
			Error::<Test>::UnknownRegion
		);

		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_noop!(
			<Broker as Mutate<_>>::mint_into(&region_id.into(), &2),
			Error::<Test>::NotAllowed
		);

		assert_noop!(
			<Broker as Mutate<_>>::burn(&region_id.into(), Some(&2)),
			Error::<Test>::NotOwner
		);
		// 'withdraw' the region from user 1:
		assert_ok!(<Broker as Mutate<_>>::burn(&region_id.into(), Some(&1)));
		assert_eq!(Regions::<Test>::get(region_id).unwrap().owner, None);

		// `mint_into` works after burning:
		assert_ok!(<Broker as Mutate<_>>::mint_into(&region_id.into(), &2));
		assert_eq!(Regions::<Test>::get(region_id).unwrap().owner, Some(2));

		// Unsupported operations:
		assert_noop!(
			<Broker as Mutate<_>>::set_attribute(&region_id.into(), &[], &[]),
			TokenError::Unsupported
		);
		assert_noop!(
			<Broker as Mutate<_>>::set_typed_attribute::<u8, u8>(&region_id.into(), &0, &0),
			TokenError::Unsupported
		);
	});
}

#[test]
fn mutate_operations_work_with_partitioned_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, _region2) = Broker::do_partition(region, None, 2).unwrap();
		let record_1 = Regions::<Test>::get(region1).unwrap();

		// 'withdraw' the region from user 1:
		assert_ok!(<Broker as Mutate<_>>::burn(&region1.into(), Some(&1)));
		assert_eq!(Regions::<Test>::get(region1).unwrap().owner, None);

		// `mint_into` works after burning:
		assert_ok!(<Broker as Mutate<_>>::mint_into(&region1.into(), &1));

		// Ensure the region minted is the same as the one we burned previously:
		assert_eq!(Regions::<Test>::get(region1).unwrap(), record_1);
	});
}

#[test]
fn mutate_operations_work_with_interlaced_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, _region2) =
			Broker::do_interlace(region, None, CoreMask::from_chunk(0, 40)).unwrap();
		let record_1 = Regions::<Test>::get(region1).unwrap();

		// 'withdraw' the region from user 1:
		assert_ok!(<Broker as Mutate<_>>::burn(&region1.into(), Some(&1)));
		assert_eq!(Regions::<Test>::get(region1).unwrap().owner, None);

		// `mint_into` works after burning:
		assert_ok!(<Broker as Mutate<_>>::mint_into(&region1.into(), &1));

		// Ensure the region minted is the same as the one we burned previously:
		assert_eq!(Regions::<Test>::get(region1).unwrap(), record_1);
	});
}

#[test]
fn permanent_is_not_reassignable() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, Some(1), 1001, Final));
		assert_noop!(Broker::do_assign(region, Some(1), 1002, Final), Error::<Test>::UnknownRegion);
		assert_noop!(Broker::do_pool(region, Some(1), 1002, Final), Error::<Test>::UnknownRegion);
		assert_noop!(Broker::do_partition(region, Some(1), 1), Error::<Test>::UnknownRegion);
		assert_noop!(
			Broker::do_interlace(region, Some(1), CoreMask::from_chunk(0, 40)),
			Error::<Test>::UnknownRegion
		);
	});
}

#[test]
fn provisional_is_reassignable() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, Some(1), 1001, Provisional));
		let (region1, region) = Broker::do_partition(region, Some(1), 1).unwrap();
		let (region2, region3) =
			Broker::do_interlace(region, Some(1), CoreMask::from_chunk(0, 40)).unwrap();
		assert_ok!(Broker::do_pool(region1, Some(1), 1, Provisional));
		assert_ok!(Broker::do_assign(region2, Some(1), 1002, Provisional));
		assert_ok!(Broker::do_assign(region3, Some(1), 1003, Provisional));
		advance_to(8);
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Pool, 57600),],
						end_hint: None
					}
				),
				(
					8,
					AssignCore {
						core: 0,
						begin: 10,
						assignment: vec![(Task(1002), 28800), (Task(1003), 28800),],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn nft_metadata_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(attribute::<Timeslice>(region, b"begin"), 4);
		assert_eq!(attribute::<Timeslice>(region, b"length"), 3);
		assert_eq!(attribute::<Timeslice>(region, b"end"), 7);
		assert_eq!(attribute::<Option<u64>>(region, b"owner"), Some(1));
		assert_eq!(attribute::<CoreMask>(region, b"part"), 0xfffff_fffff_fffff_fffff.into());
		assert_eq!(attribute::<CoreIndex>(region, b"core"), 0);
		assert_eq!(attribute::<Option<u64>>(region, b"paid"), Some(100));

		assert_ok!(Broker::do_transfer(region, None, 42));
		let (_, region) = Broker::do_partition(region, None, 2).unwrap();
		let (region, _) =
			Broker::do_interlace(region, None, 0x00000_fffff_fffff_00000.into()).unwrap();
		assert_eq!(attribute::<Timeslice>(region, b"begin"), 6);
		assert_eq!(attribute::<Timeslice>(region, b"length"), 1);
		assert_eq!(attribute::<Timeslice>(region, b"end"), 7);
		assert_eq!(attribute::<Option<u64>>(region, b"owner"), Some(42));
		assert_eq!(attribute::<CoreMask>(region, b"part"), 0x00000_fffff_fffff_00000.into());
		assert_eq!(attribute::<CoreIndex>(region, b"core"), 0);
		assert_eq!(attribute::<Option<u64>>(region, b"paid"), None);
	});
}

#[test]
fn migration_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_set_lease(1000, 8));
		assert_ok!(Broker::do_start_sales(100, 1));

		// Sale is for regions from TS4..7
		// Not ending in this sale period.
		assert_noop!(Broker::do_renew(1, 0), Error::<Test>::NotAllowed);

		advance_to(12);
		// Sale is now for regions from TS10..13
		// Ending in this sale period.
		// Should now be renewable.
		assert_ok!(Broker::do_renew(1, 0));
		assert_eq!(balance(1), 900);
		advance_to(18);

		let just_pool = || vec![(Pool, 57600)];
		let just_1000 = || vec![(Task(1000), 57600)];
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(6, AssignCore { core: 0, begin: 8, assignment: just_1000(), end_hint: None }),
				(6, AssignCore { core: 1, begin: 8, assignment: just_pool(), end_hint: None }),
				(12, AssignCore { core: 0, begin: 14, assignment: just_1000(), end_hint: None }),
				(12, AssignCore { core: 1, begin: 14, assignment: just_pool(), end_hint: None }),
				(18, AssignCore { core: 0, begin: 20, assignment: just_1000(), end_hint: None }),
				(18, AssignCore { core: 1, begin: 20, assignment: just_pool(), end_hint: None }),
			]
		);
	});
}

#[test]
fn migration_v5_reconstructs_sale_index_from_region_begin() {
	use crate::migration::v5::{old, FirstSaleRegion, MigrateToV5Impl};
	use frame_support::traits::UncheckedOnRuntimeUpgrade;

	struct FirstRegion;
	impl FirstSaleRegion for FirstRegion {
		fn region_begin() -> Timeslice {
			100
		}
	}

	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		let region_length = Configuration::<Test>::get().unwrap().region_length;
		let template = SaleInfo::<Test>::get().unwrap();

		let migrate_with_region_begin = |region_begin: Timeslice| {
			// Write the pre-v5 record (old layout, no `sale_index`) that the migration reads.
			old::SaleInfo::<Test>::put(old::SaleInfoRecord {
				sale_start: template.sale_start,
				leadin_length: template.leadin_length,
				end_price: template.end_price,
				region_begin,
				region_end: region_begin + region_length,
				ideal_cores_sold: template.ideal_cores_sold,
				cores_offered: template.cores_offered,
				first_core: template.first_core,
				sellout_price: template.sellout_price,
				cores_sold: template.cores_sold,
			});

			let _ = <MigrateToV5Impl<Test, FirstRegion> as UncheckedOnRuntimeUpgrade>::on_runtime_upgrade();
			SaleInfo::<Test>::get().unwrap().sale_index
		};

		// First sale (region_begin == anchor) is index 1; bootstrap sale is index 0.
		assert_eq!(migrate_with_region_begin(100), 1);

		// Five regions later the current sale is index 6.
		assert_eq!(migrate_with_region_begin(100 + 5 * region_length), 6);
	});
}

#[test]
fn migration_v5_defaults_sale_index_when_configuration_missing() {
	use crate::migration::v5::{old, FirstSaleRegion, MigrateToV5Impl};
	use frame_support::traits::UncheckedOnRuntimeUpgrade;
	use sp_tracing::{
		test_log_capture::init_log_capture,
		tracing::{subscriber, Level},
	};

	struct FirstRegion;
	impl FirstSaleRegion for FirstRegion {
		fn region_begin() -> Timeslice {
			100
		}
	}

	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		let region_length = Configuration::<Test>::get().unwrap().region_length;
		let template = SaleInfo::<Test>::get().unwrap();

		// Pre-v5 record present, Configuration gone: migration must not panic, still rewrites it.
		old::SaleInfo::<Test>::put(old::SaleInfoRecord {
			sale_start: template.sale_start,
			leadin_length: template.leadin_length,
			end_price: template.end_price,
			region_begin: 100 + 5 * region_length,
			region_end: 100 + 6 * region_length,
			ideal_cores_sold: template.ideal_cores_sold,
			cores_offered: template.cores_offered,
			first_core: template.first_core,
			sellout_price: template.sellout_price,
			cores_sold: template.cores_sold,
		});
		Configuration::<Test>::kill();

		// `log` to `tracing` bridge, needed for capture.
		sp_tracing::init_for_tests();
		let (log_capture, subscriber) = init_log_capture(Level::ERROR, false);
		subscriber::with_default(subscriber, || {
			let _ = <MigrateToV5Impl<Test, FirstRegion> as UncheckedOnRuntimeUpgrade>::on_runtime_upgrade();
		});

		// Record still rewritten, with the fallback index, and the fallback is logged.
		let migrated = SaleInfo::<Test>::get().expect("record rewritten in new layout");
		assert_eq!(migrated.sale_index, 1);
		assert!(log_capture.contains("Configuration missing while SaleInfo exists"));
	});
}

#[cfg(feature = "try-runtime")]
#[test]
fn migration_v5_post_upgrade_accepts_missing_sale_info() {
	use crate::migration::v5::{FirstSaleRegion, MigrateToV5Impl};
	use frame_support::traits::UncheckedOnRuntimeUpgrade;

	struct FirstRegion;
	impl FirstSaleRegion for FirstRegion {
		fn region_begin() -> Timeslice {
			100
		}
	}

	type Migration = MigrateToV5Impl<Test, FirstRegion>;

	TestExt::new().execute_with(|| {
		// No SaleInfo: on_runtime_upgrade skips, so pre/post_upgrade must agree it is valid.
		assert!(SaleInfo::<Test>::get().is_none());

		let state = <Migration as UncheckedOnRuntimeUpgrade>::pre_upgrade().unwrap();
		let _ = <Migration as UncheckedOnRuntimeUpgrade>::on_runtime_upgrade();
		assert!(SaleInfo::<Test>::get().is_none());
		assert_ok!(<Migration as UncheckedOnRuntimeUpgrade>::post_upgrade(state));
	});
}

#[test]
fn renewal_works() {
	let b = 100_000;
	TestExt::new().endow(1, b).execute_with(move || {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(balance(1), 99_900);
		assert_ok!(Broker::do_assign(region, None, 1001, Final));
		// Should now be renewable.
		advance_to(6);
		assert_noop!(Broker::do_purchase(1, u64::max_value()), Error::<Test>::TooEarly);
		let core = Broker::do_renew(1, region.core).unwrap();
		assert_eq!(balance(1), 99_800);
		advance_to(8);
		assert_noop!(Broker::do_purchase(1, u64::max_value()), Error::<Test>::SoldOut);
		advance_to(12);
		assert_ok!(Broker::do_renew(1, core));
		assert_eq!(balance(1), 99_690);
	});
}

#[test]
/// Renewals have to affect price as well. Otherwise a market where everything is a renewal would
/// not work. Renewals happening in the leadin or after are effectively competing with the open
/// market and it makes sense to adjust the price to what was paid here. Assuming all renewals were
/// done in the interlude and only normal sales happen in the leadin, renewals will have no effect
/// on price. If there are no cores left for sale on the open markent, renewals will affect price
/// even in the interlude, making sure renewal prices stay in the range of the open market.
fn renewals_affect_price() {
	sp_tracing::try_init_simple();
	let b = 100_000;
	let config = ConfigRecord {
		advance_notice: 2,
		interlude_length: 10,
		leadin_length: 20,
		ideal_bulk_proportion: Perbill::from_percent(100),
		limit_cores_offered: None,
		// Region length is in time slices (2 blocks):
		region_length: 20,
		renewal_bump: Perbill::from_percent(10),
		contribution_timeout: 5,
	};
	TestExt::new_with_config(config).endow(1, b).execute_with(|| {
		let price = 910;
		assert_ok!(Broker::do_start_sales(10, 1));
		advance_to(11);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		// Price is lower, because already one block in:
		let b = b - price;
		assert_eq!(balance(1), b);
		assert_ok!(Broker::do_assign(region, None, 1001, Final));
		advance_to(40);
		assert_noop!(Broker::do_purchase(1, u64::max_value()), Error::<Test>::TooEarly);
		let core = Broker::do_renew(1, region.core).unwrap();
		// First renewal has same price as initial purchase.
		let b = b - price;
		assert_eq!(balance(1), b);
		advance_to(51);
		assert_noop!(Broker::do_purchase(1, u64::max_value()), Error::<Test>::SoldOut);
		advance_to(81);
		assert_ok!(Broker::do_renew(1, core));
		// Renewal bump in effect
		let price = price + Perbill::from_percent(10) * price;
		let b = b - price;
		assert_eq!(balance(1), b);

		// Move after interlude and leadin - should reduce price.
		advance_to(159);
		Broker::do_renew(1, region.core).unwrap();
		let price = price + Perbill::from_percent(10) * price;
		let b = b - price;
		assert_eq!(balance(1), b);

		advance_to(161);
		// Should have the reduced price now:
		Broker::do_renew(1, region.core).unwrap();
		let price = 100;
		let b = b - price;
		assert_eq!(balance(1), b);

		// Price should be bumped normally again:
		advance_to(201);
		Broker::do_renew(1, region.core).unwrap();
		let price = 110;
		let b = b - price;
		assert_eq!(balance(1), b);
	});
}

#[test]
/// Renewals adjust to lower end of market
fn renewal_price_adjusts_to_lower_market_end() {
	sp_tracing::try_init_simple();
	let b = 100_000_000;
	let region_length_blocks = 40;
	let config = ConfigRecord {
		advance_notice: 2,
		interlude_length: 10,
		leadin_length: 20,
		ideal_bulk_proportion: Perbill::from_percent(100),
		limit_cores_offered: None,
		// Region length is in time slices (2 blocks):
		region_length: 20,
		renewal_bump: Perbill::from_percent(10),
		contribution_timeout: 5,
	};
	TestExt::new_with_config(config.clone())
		.endow(1, b)
		.endow(2, b)
		.execute_with(|| {
			let price = 910;
			assert_ok!(Broker::do_start_sales(10, 2));
			advance_to(11);
			let region = Broker::do_purchase(1, u64::max_value()).unwrap();
			// Price is lower, because already one block in:
			let b = b - price;
			assert_eq!(balance(1), b);
			assert_ok!(Broker::do_assign(region, None, 1001, Final));
			advance_to(region_length_blocks);
			assert_noop!(Broker::do_purchase(1, u64::max_value()), Error::<Test>::TooEarly);

			let core = Broker::do_renew(1, region.core).unwrap();
			// First renewal has same price as initial purchase.
			let b = b - price;
			assert_eq!(balance(1), b);
			// Ramp up price:
			advance_to(region_length_blocks + config.interlude_length + 1);
			Broker::do_purchase(2, u64::max_value()).unwrap();

			advance_to(2 * region_length_blocks);
			assert_ok!(Broker::do_renew(1, core));
			// Renewal bump in effect
			let price = price + Perbill::from_percent(10) * price;
			let b = b - price;
			assert_eq!(balance(1), b);
			// Ramp up price again:
			advance_to(2 * region_length_blocks + config.interlude_length + 1);
			Broker::do_purchase(2, u64::max_value()).unwrap();

			advance_to(3 * region_length_blocks);
			assert_ok!(Broker::do_renew(1, core));
			// Renewal bump still in effect
			let price = price + Perbill::from_percent(10) * price;
			let b = b - price;
			assert_eq!(balance(1), b);
			// No further price ramp up necessary - the price of this sale is relevant for next
			// renewal.
			let end_price = SaleInfo::<Test>::get().unwrap().end_price;

			advance_to(4 * region_length_blocks);
			assert_ok!(Broker::do_renew(1, core));
			// Renewal bump trumped by end price of previous sale.
			let price = end_price;
			let b = b - price;
			assert_eq!(balance(1), b);
		});
}

#[test]
fn instapool_payouts_work() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		let item = ScheduleItem { assignment: Pool, mask: CoreMask::complete() };
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(vec![item])));
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(revenue(), 100);
		assert_ok!(Broker::do_pool(region, None, 2, Final));
		assert_ok!(Broker::do_purchase_credit(1, 20, 1));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 100);
		advance_to(8);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 10));
		advance_to(11);
		// Should get revenue amount 10 from RC, from which 6 is system payout (goes to account0
		// instantly) and the rest is private (kept in the pot until claimed)
		assert_eq!(pot(), 4);
		assert_eq!(revenue(), 106);

		// Cannot claim for 0 timeslices.
		assert_noop!(Broker::do_claim_revenue(region, 0), Error::<Test>::NoClaimTimeslices);

		// Revenue can be claimed.
		assert_ok!(Broker::do_claim_revenue(region, 100));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 106);
		assert_eq!(balance(2), 4);
	});
}

#[test]
fn instapool_partial_core_payouts_work() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		let item = ScheduleItem { assignment: Pool, mask: CoreMask::complete() };
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(vec![item])));
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, region2) =
			Broker::do_interlace(region, None, CoreMask::from_chunk(0, 20)).unwrap();
		assert_ok!(Broker::do_pool(region1, None, 2, Final));
		assert_ok!(Broker::do_pool(region2, None, 3, Final));
		// Buy and spend 40 credits to make the interlaced region payouts a nice round number.
		assert_ok!(Broker::do_purchase_credit(1, 40, 1));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 100);
		advance_to(8);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 40));
		advance_to(11);
		// Half the revenue goes to the private pot which can then be claimed.
		assert_eq!(pot(), 20);
		assert_ok!(Broker::do_claim_revenue(region1, 100));
		assert_ok!(Broker::do_claim_revenue(region2, 100));
		// Then the private pot is split 20:60 due to the interlacing pattern.
		assert_eq!(balance(2), 5);
		assert_eq!(balance(3), 15);
		// And the bookkeeping is correct.
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 120);
	});
}

#[test]
fn instapool_core_payouts_work_with_partitioned_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(revenue(), 100);
		let (region1, region2) = Broker::do_partition(region, None, 2).unwrap();
		// `region1` duration is from rcblock 8 to rcblock 12. This means that the
		// coretime purchased during this time period will be purchased from `region1`
		//
		// `region2` duration is from rcblock 12 to rcblock 14 and during this period
		// coretime will be purchased from `region2`.
		assert_ok!(Broker::do_pool(region1, None, 2, Final));
		assert_ok!(Broker::do_pool(region2, None, 3, Final));
		assert_ok!(Broker::do_purchase_credit(1, 20, 1));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 100);
		advance_to(8);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 10));
		advance_to(11);
		assert_eq!(pot(), 10);
		assert_eq!(revenue(), 100);
		assert_ok!(Broker::do_claim_revenue(region1, 100));
		assert_eq!(pot(), 0);
		assert_eq!(balance(2), 10);
		advance_to(12);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 10));
		advance_to(15);
		assert_eq!(pot(), 10);
		assert_ok!(Broker::do_claim_revenue(region2, 100));
		assert_eq!(pot(), 0);
		// The balance of account `2` remains unchanged.
		assert_eq!(balance(2), 10);
		assert_eq!(balance(3), 10);
	});
}

#[test]
fn instapool_payouts_cannot_be_duplicated_through_partition() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		let item = ScheduleItem { assignment: Pool, mask: CoreMask::complete() };
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(vec![item])));
		assert_ok!(Broker::do_start_sales(100, 3));
		advance_to(2);

		// Buy core to add to pool. This adds 100 to revenue.
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(revenue(), 100);

		// Ensure InstaPoolIo corresponds to one full region provided by the system.
		let region = Regions::<Test>::get(&region_id).unwrap();
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 0, system: 80 }
		);
		assert_eq!(InstaPoolIo::<Test>::get(region.end), PoolIoRecord { private: 0, system: -80 });

		// Add region to pool with Provisional finality.
		assert_ok!(Broker::do_pool(region_id, None, 2, Provisional));
		// Contribution exists for the full region.
		assert_eq!(
			InstaPoolContribution::<Test>::get(region_id),
			Some(ContributionRecord { length: 3, payee: 2 })
		);
		// Pool IO registers this region entering and exiting at the correct points.
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 80, system: 80 }
		);
		assert_eq!(
			InstaPoolIo::<Test>::get(region.end),
			PoolIoRecord { private: -80, system: -80 }
		);

		// Region can still be partitioned, which replaces the old region with two new ones.
		assert_ok!(Broker::do_partition(region_id, None, 1));

		// Old region is removed from contributions and accounted for by pool IO.
		assert_eq!(InstaPoolContribution::<Test>::get(region_id), None);
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 0, system: 80 }
		);
		assert_eq!(InstaPoolIo::<Test>::get(region.end), PoolIoRecord { private: 0, system: -80 });

		// Add some revenue.
		assert_ok!(Broker::do_purchase_credit(1, 20, 1));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 100);
		advance_to(8);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 10));
		advance_to(11);
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 110);

		// Revenue cannot be claimed for the old region.
		assert_noop!(Broker::do_claim_revenue(region_id, 100), Error::<Test>::UnknownContribution);
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 110);
		assert_eq!(balance(2), 0);
	});
}

#[test]
fn insta_pool_history_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		// We'll be calling get() on this a lot.
		type Io = InstaPoolIo<Test>;
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);

		// Buy core to add to pool.
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();

		// Ensure InstaPoolIo is zeroed.
		let region = Regions::<Test>::get(&region_id).unwrap();
		assert_eq!(Io::get(region_id.begin), PoolIoRecord { private: 0, system: 0 });
		assert_eq!(Io::get(region.end), PoolIoRecord { private: 0, system: 0 });

		assert_eq!(region_id.begin, 4);

		// Add region to pool with Provisional finality.
		assert_ok!(Broker::do_pool(region_id, None, 2, Provisional));
		// Pool IO registers this region entering and exiting at the correct points.
		assert_eq!(Io::get(region_id.begin), PoolIoRecord { private: 80, system: 0 });
		assert_eq!(Io::get(region.end), PoolIoRecord { private: -80, system: 0 });

		// Ensure the history is correct for a full region. Starts at Timeslice 1 with no capacity
		// (Some(0)) for a region (3 timeslices). Timeslice 4 is the region that we put into the
		// pool, this gives us 80 blocks of on-demand per timeslice for a region (three timeslices).
		// Then we go back to Some(0) when it is removed.
		let timeslice_period: u64 = <Test as Config>::TimeslicePeriod::get();
		let expected_private_history = vec![0, 0, 0, 80, 80, 80, 0];

		// Advance and collate the history starting from the current timeslice.
		let actual_private_history: Vec<_> = (1..8)
			.map(|timeslice| {
				advance_to(timeslice as u64 * timeslice_period);
				InstaPoolHistory::<Test>::get(timeslice).unwrap().private_contributions
			})
			.collect();
		assert_eq!(actual_private_history, expected_private_history);

		// Check the events are emitted and agree.
		System::assert_has_event(
			Event::HistoryInitialized { when: 1, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		System::assert_has_event(
			Event::HistoryInitialized { when: 2, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		System::assert_has_event(
			Event::HistoryInitialized { when: 3, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		// Region is pooled starting in timeslice 4 for three timeslices (a region length).
		System::assert_has_event(
			Event::HistoryInitialized { when: 4, private_pool_size: 80, system_pool_size: 0 }
				.into(),
		);
		System::assert_has_event(
			Event::HistoryInitialized { when: 5, private_pool_size: 80, system_pool_size: 0 }
				.into(),
		);
		System::assert_has_event(
			Event::HistoryInitialized { when: 6, private_pool_size: 80, system_pool_size: 0 }
				.into(),
		);
		// The contributed region has ended and the unsold core is pooled by the system.
		System::assert_has_event(
			Event::HistoryInitialized { when: 7, private_pool_size: 0, system_pool_size: 80 }
				.into(),
		);
	});
}

#[test]
fn force_unpool_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		// We'll be calling get() on this a lot.
		type Io = InstaPoolIo<Test>;
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);

		// Started with nothing in pool.
		System::assert_has_event(
			Event::HistoryInitialized { when: 1, private_pool_size: 0, system_pool_size: 0 }.into(),
		);

		// Buy core to add to pool.
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();

		// Ensure InstaPoolIo is zeroed.
		let region = Regions::<Test>::get(&region_id).unwrap();
		assert_eq!(Io::get(region_id.begin), PoolIoRecord { private: 0, system: 0 });
		assert_eq!(Io::get(region.end), PoolIoRecord { private: 0, system: 0 });

		// Add region to pool with Provisional finality.
		assert_ok!(Broker::do_pool(region_id, None, 2, Provisional));
		// Pool IO registers this region entering and exiting at the correct points.
		assert_eq!(Io::get(region_id.begin), PoolIoRecord { private: 80, system: 0 });
		assert_eq!(Io::get(region.end), PoolIoRecord { private: -80, system: 0 });

		// Force unpool before the region begins.
		let status = Status::<Test>::get().unwrap();
		Broker::force_unpool_region(region_id, &region, &status);
		System::assert_last_event(
			Event::<Test>::RegionUnpooled { region_id, when: region_id.begin }.into(),
		);
		// Pool IO does not change now.
		assert_eq!(Io::get(Broker::current_timeslice()), PoolIoRecord { private: 0, system: 0 });
		// But changes at the point of the region beginning.
		assert_eq!(Io::get(region_id.begin), PoolIoRecord { private: 0, system: 0 });
		// History is never initialized.
		InstaPoolHistory::<Test>::get(Broker::current_timeslice())
			.map(|record| record.private_contributions);

		// Pool it again.
		assert_ok!(Broker::do_pool(region_id, None, 2, Provisional));
		assert_eq!(Io::get(region_id.begin), PoolIoRecord { private: 80, system: 0 });

		// Advance to the timeslice after the region starts.
		let timeslice_period: u64 = <Test as Config>::TimeslicePeriod::get();
		advance_to(3 * timeslice_period);
		let current_timeslice = Broker::current_timeslice();

		System::assert_has_event(
			Event::HistoryInitialized { when: 2, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		System::assert_has_event(
			Event::HistoryInitialized { when: 3, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		// This is the only timeslice that actually made it into the pool.
		System::assert_has_event(
			Event::HistoryInitialized { when: 4, private_pool_size: 80, system_pool_size: 0 }
				.into(),
		);

		// Check the Io right now at key timeslices and then force unpool.
		assert_eq!(Io::get(region.end), PoolIoRecord { private: -80, system: 0 });
		assert_eq!(Io::get(current_timeslice), PoolIoRecord { private: 0, system: 0 });
		let status = Status::<Test>::get().unwrap();
		Broker::force_unpool_region(region_id, &region, &status);

		// Check that it is unpooled from the next uncommitted timeslice.
		System::assert_last_event(
			Event::<Test>::RegionUnpooled { region_id, when: current_timeslice + 2 }.into(),
		);
		// Ensure nothing removed at the end of the region.
		assert_eq!(Io::get(region.end), PoolIoRecord { private: 0, system: 0 });
		// And is instead removed the next uncommitted timeslice.
		assert_eq!(Io::get(current_timeslice + 2), PoolIoRecord { private: -80, system: 0 });

		// Check that the history agrees.
		advance_sale_period();
		// The rest should account for the fact we removed it in time for timeslice 5.
		System::assert_has_event(
			Event::HistoryInitialized { when: 5, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		System::assert_has_event(
			Event::HistoryInitialized { when: 6, private_pool_size: 0, system_pool_size: 0 }.into(),
		);
		// rotate_sale pools the core that was not bought the previous sale.
		System::assert_has_event(
			Event::HistoryInitialized { when: 7, private_pool_size: 0, system_pool_size: 80 }
				.into(),
		);
	});
}

#[test]
fn instapool_payouts_cannot_be_duplicated_through_interlacing() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		let item = ScheduleItem { assignment: Pool, mask: CoreMask::complete() };
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(vec![item])));
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);

		// Buy core to add to pool. This adds 100 to revenue.
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(revenue(), 100);

		// Ensure InstaPoolIo corresponds to one full region provided by the system.
		let region = Regions::<Test>::get(&region_id).unwrap();
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 0, system: 80 }
		);
		assert_eq!(InstaPoolIo::<Test>::get(region.end), PoolIoRecord { private: 0, system: -80 });

		// Add region to pool with Provisional finality.
		assert_ok!(Broker::do_pool(region_id, None, 2, Provisional));
		// Contribution exists for the full region.
		assert_eq!(
			InstaPoolContribution::<Test>::get(region_id),
			Some(ContributionRecord { length: 3, payee: 2 })
		);
		// Pool IO registers this region entering and exiting at the correct points.
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 80, system: 80 }
		);
		assert_eq!(
			InstaPoolIo::<Test>::get(region.end),
			PoolIoRecord { private: -80, system: -80 }
		);

		// Region can still be interlaced, which replaces the old region with two new ones.
		assert_ok!(Broker::do_interlace(region_id, None, 0xfffff_fffff_00000_00000.into()));

		// Old region is removed from contributions and accounted for by pool IO.
		assert_eq!(InstaPoolContribution::<Test>::get(region_id), None);
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 0, system: 80 }
		);
		assert_eq!(InstaPoolIo::<Test>::get(region.end), PoolIoRecord { private: 0, system: -80 });

		// Add some revenue.
		assert_ok!(Broker::do_purchase_credit(1, 20, 1));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 100);
		advance_to(8);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 10));
		// Pot is still zero and the 10 is all system revenue.
		advance_to(11);
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 110);

		// Revenue cannot be claimed for the old region.
		assert_noop!(Broker::do_claim_revenue(region_id, 100), Error::<Test>::UnknownContribution);
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 110);
		assert_eq!(balance(2), 0);
	});
}

#[test]
fn instapool_payouts_cannot_be_duplicated_through_reassignment() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		let item = ScheduleItem { assignment: Pool, mask: CoreMask::complete() };
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(vec![item])));
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);

		// Buy core to add to pool. This adds 100 to revenue.
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_eq!(revenue(), 100);

		// Ensure InstaPoolIo corresponds to one full region provided by the system.
		let region = Regions::<Test>::get(&region_id).unwrap();
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 0, system: 80 }
		);
		assert_eq!(InstaPoolIo::<Test>::get(region.end), PoolIoRecord { private: 0, system: -80 });

		// Add region to pool with Provisional finality.
		assert_ok!(Broker::do_pool(region_id, None, 2, Provisional));
		// Contribution exists for the full region.
		assert_eq!(
			InstaPoolContribution::<Test>::get(region_id),
			Some(ContributionRecord { length: 3, payee: 2 })
		);
		// Pool IO registers this region entering and exiting at the correct points.
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 80, system: 80 }
		);
		assert_eq!(
			InstaPoolIo::<Test>::get(region.end),
			PoolIoRecord { private: -80, system: -80 }
		);

		// Region can still be reassigned.
		assert_ok!(Broker::do_assign(region_id, None, 2000, Finality::Final));

		// The region is removed from contributions and accounted for by pool IO.
		assert_eq!(InstaPoolContribution::<Test>::get(region_id), None);
		assert_eq!(
			InstaPoolIo::<Test>::get(region_id.begin),
			PoolIoRecord { private: 0, system: 80 }
		);
		assert_eq!(InstaPoolIo::<Test>::get(region.end), PoolIoRecord { private: 0, system: -80 });

		// Add some revenue.
		assert_ok!(Broker::do_purchase_credit(1, 20, 1));
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 100);
		advance_to(8);
		assert_ok!(TestCoretimeProvider::spend_instantaneous(1, 10));
		// Pot is still zero and the 10 is all system revenue.
		advance_to(11);
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 110);

		// Revenue cannot be claimed for the reassigned region.
		assert_noop!(Broker::do_claim_revenue(region_id, 100), Error::<Test>::UnknownContribution);
		assert_eq!(pot(), 0);
		assert_eq!(revenue(), 110);
		assert_eq!(balance(2), 0);
	});
}

#[test]
fn initialize_with_system_paras_works() {
	TestExt::new().execute_with(|| {
		let item = ScheduleItem { assignment: Task(1u32), mask: CoreMask::complete() };
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(vec![item])));
		let items = vec![
			ScheduleItem { assignment: Task(2u32), mask: 0xfffff_fffff_00000_00000.into() },
			ScheduleItem { assignment: Task(3u32), mask: 0x00000_00000_fffff_00000.into() },
			ScheduleItem { assignment: Task(4u32), mask: 0x00000_00000_00000_fffff.into() },
		];
		assert_ok!(Broker::do_reserve(Schedule::truncate_from(items)));
		assert_ok!(Broker::do_start_sales(100, 0));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1), 57600),],
						end_hint: None
					}
				),
				(
					6,
					AssignCore {
						core: 1,
						begin: 8,
						assignment: vec![(Task(2), 28800), (Task(3), 14400), (Task(4), 14400),],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn initialize_with_leased_slots_works() {
	TestExt::new().execute_with(|| {
		assert_ok!(Broker::do_set_lease(1000, 6));
		assert_ok!(Broker::do_set_lease(1001, 7));
		assert_ok!(Broker::do_start_sales(100, 0));
		advance_to(18);
		let end_hint = None;
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1000), 57600),],
						end_hint
					}
				),
				(
					6,
					AssignCore {
						core: 1,
						begin: 8,
						assignment: vec![(Task(1001), 57600),],
						end_hint
					}
				),
				(
					12,
					AssignCore {
						core: 0,
						begin: 14,
						assignment: vec![(Task(1001), 57600),],
						end_hint
					}
				),
				(12, AssignCore { core: 1, begin: 14, assignment: vec![(Pool, 57600),], end_hint }),
				(18, AssignCore { core: 0, begin: 20, assignment: vec![(Pool, 57600),], end_hint }),
				(18, AssignCore { core: 1, begin: 20, assignment: vec![(Pool, 57600),], end_hint }),
			]
		);
	});
}

#[test]
fn purchase_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, None, 1000, Final));
		advance_to(6);
		assert_eq!(
			CoretimeTrace::get(),
			vec![(
				6,
				AssignCore {
					core: 0,
					begin: 8,
					assignment: vec![(Task(1000), 57600),],
					end_hint: None
				}
			),]
		);
	});
}

#[test]
fn purchase_credit_works() {
	TestExt::new().endow(1, 50).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);

		let credits = CoretimeCredit::get();
		assert_eq!(credits.get(&1), None);

		assert_noop!(Broker::do_purchase_credit(1, 10, 1), Error::<Test>::CreditPurchaseTooSmall);
		assert_noop!(Broker::do_purchase_credit(1, 100, 1), TokenError::FundsUnavailable);

		assert_ok!(Broker::do_purchase_credit(1, 50, 1));
		let credits = CoretimeCredit::get();
		assert_eq!(credits.get(&1), Some(&50));
	});
}

#[test]
fn partition_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, region) = Broker::do_partition(region, None, 1).unwrap();
		let (region2, region3) = Broker::do_partition(region, None, 1).unwrap();
		assert_ok!(Broker::do_assign(region1, None, 1001, Final));
		assert_ok!(Broker::do_assign(region2, None, 1002, Final));
		assert_ok!(Broker::do_assign(region3, None, 1003, Final));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1001), 57600),],
						end_hint: None
					}
				),
				(
					8,
					AssignCore {
						core: 0,
						begin: 10,
						assignment: vec![(Task(1002), 57600),],
						end_hint: None
					}
				),
				(
					10,
					AssignCore {
						core: 0,
						begin: 12,
						assignment: vec![(Task(1003), 57600),],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn interlace_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, region) =
			Broker::do_interlace(region, None, CoreMask::from_chunk(0, 30)).unwrap();
		let (region2, region3) =
			Broker::do_interlace(region, None, CoreMask::from_chunk(30, 60)).unwrap();
		assert_ok!(Broker::do_assign(region1, None, 1001, Final));
		assert_ok!(Broker::do_assign(region2, None, 1002, Final));
		assert_ok!(Broker::do_assign(region3, None, 1003, Final));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![(
				6,
				AssignCore {
					core: 0,
					begin: 8,
					assignment: vec![(Task(1001), 21600), (Task(1002), 21600), (Task(1003), 14400),],
					end_hint: None
				}
			),]
		);
	});
}

#[test]
fn cant_assign_unowned_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, region2) =
			Broker::do_interlace(region, Some(1), CoreMask::from_chunk(0, 30)).unwrap();

		// Transfer the interlaced region to account 2.
		assert_ok!(Broker::do_transfer(region2, Some(1), 2));

		// The initial owner should not be able to assign the non-interlaced region, since they have
		// just transferred an interlaced part of it to account 2.
		assert_noop!(Broker::do_assign(region, Some(1), 1001, Final), Error::<Test>::UnknownRegion);

		// Account 1 can assign only the interlaced region that they did not transfer.
		assert_ok!(Broker::do_assign(region1, Some(1), 1001, Final));
		// Account 2 can assign the region they received.
		assert_ok!(Broker::do_assign(region2, Some(2), 1002, Final));

		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![(
				6,
				AssignCore {
					core: 0,
					begin: 8,
					assignment: vec![(Task(1001), 21600), (Task(1002), 36000)],
					end_hint: None
				}
			),]
		);
	});
}

#[test]
fn interlace_then_partition_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, region2) =
			Broker::do_interlace(region, None, CoreMask::from_chunk(0, 20)).unwrap();
		let (region1, region3) = Broker::do_partition(region1, None, 1).unwrap();
		let (region2, region4) = Broker::do_partition(region2, None, 2).unwrap();
		assert_ok!(Broker::do_assign(region1, None, 1001, Final));
		assert_ok!(Broker::do_assign(region2, None, 1002, Final));
		assert_ok!(Broker::do_assign(region3, None, 1003, Final));
		assert_ok!(Broker::do_assign(region4, None, 1004, Final));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1001), 14400), (Task(1002), 43200),],
						end_hint: None
					}
				),
				(
					8,
					AssignCore {
						core: 0,
						begin: 10,
						assignment: vec![(Task(1002), 43200), (Task(1003), 14400),],
						end_hint: None
					}
				),
				(
					10,
					AssignCore {
						core: 0,
						begin: 12,
						assignment: vec![(Task(1003), 14400), (Task(1004), 43200),],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn partition_then_interlace_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, region2) = Broker::do_partition(region, None, 1).unwrap();
		let (region1, region3) =
			Broker::do_interlace(region1, None, CoreMask::from_chunk(0, 20)).unwrap();
		let (region2, region4) =
			Broker::do_interlace(region2, None, CoreMask::from_chunk(0, 30)).unwrap();
		assert_ok!(Broker::do_assign(region1, None, 1001, Final));
		assert_ok!(Broker::do_assign(region2, None, 1002, Final));
		assert_ok!(Broker::do_assign(region3, None, 1003, Final));
		assert_ok!(Broker::do_assign(region4, None, 1004, Final));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1001), 14400), (Task(1003), 43200),],
						end_hint: None
					}
				),
				(
					8,
					AssignCore {
						core: 0,
						begin: 10,
						assignment: vec![(Task(1002), 21600), (Task(1004), 36000),],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn partitioning_after_assignment_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		// We will initially allocate a task to a purchased region, and after that
		// we will proceed to partition the region.
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, None, 1001, Provisional));
		let (_region, region1) = Broker::do_partition(region, None, 2).unwrap();
		// After the partitioning if we assign a new task to `region` the other region
		// will still be assigned to `Task(1001)`.
		assert_ok!(Broker::do_assign(region1, None, 1002, Provisional));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1001), 57600),],
						end_hint: None
					}
				),
				(
					10,
					AssignCore {
						core: 0,
						begin: 12,
						assignment: vec![(Task(1002), 57600),],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn interlacing_after_assignment_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		// We will initially allocate a task to a purchased region, and after that
		// we will proceed to interlace the region.
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, None, 1001, Provisional));
		let (region1, _region) =
			Broker::do_interlace(region, None, CoreMask::from_chunk(0, 40)).unwrap();
		// Interlacing the region won't affect the assignment. The entire region will still
		// be assigned to `Task(1001)`.
		//
		// However, after we assign a task to `region1` the `_region` won't be assigned
		// to `Task(1001)` anymore. It will become idle.
		assert_ok!(Broker::do_assign(region1, None, 1002, Provisional));
		advance_to(10);
		assert_eq!(
			CoretimeTrace::get(),
			vec![(
				6,
				AssignCore {
					core: 0,
					begin: 8,
					assignment: vec![(Idle, 28800), (Task(1002), 28800)],
					end_hint: None
				}
			),]
		);
	});
}

#[test]
fn reservations_are_limited() {
	TestExt::new().execute_with(|| {
		let schedule = Schedule::truncate_from(vec![ScheduleItem {
			assignment: Pool,
			mask: CoreMask::complete(),
		}]);
		let max_cores: u32 = <Test as Config>::MaxReservedCores::get();
		Reservations::<Test>::put(
			BoundedVec::try_from(vec![schedule.clone(); max_cores as usize]).unwrap(),
		);
		assert_noop!(Broker::do_reserve(schedule), Error::<Test>::TooManyReservations);
	});
}

#[test]
fn cannot_unreserve_unknown() {
	TestExt::new().execute_with(|| {
		let schedule = Schedule::truncate_from(vec![ScheduleItem {
			assignment: Pool,
			mask: CoreMask::complete(),
		}]);
		Reservations::<Test>::put(BoundedVec::try_from(vec![schedule.clone(); 1usize]).unwrap());
		assert_noop!(Broker::do_unreserve(2), Error::<Test>::UnknownReservation);
	});
}

#[test]
fn cannot_set_expired_lease() {
	TestExt::new().execute_with(|| {
		advance_to(2);
		let current_timeslice = Broker::current_timeslice();
		assert_noop!(
			Broker::do_set_lease(1000, current_timeslice.saturating_sub(1)),
			Error::<Test>::AlreadyExpired
		);
	});
}

#[test]
fn short_leases_are_cleaned() {
	TestExt::new().region_length(3).execute_with(|| {
		assert_ok!(Broker::do_start_sales(200, 1));
		advance_to(2);

		// New leases are allowed to expire within this region given expiry > `current_timeslice`.
		assert_noop!(
			Broker::do_set_lease(1000, Broker::current_timeslice()),
			Error::<Test>::AlreadyExpired
		);
		assert_eq!(Leases::<Test>::get().len(), 0);
		assert_ok!(Broker::do_set_lease(1000, Broker::current_timeslice().saturating_add(1)));
		assert_eq!(Leases::<Test>::get().len(), 1);

		// But are cleaned up in the next rotate_sale.
		let config = Configuration::<Test>::get().unwrap();
		let timeslice_period: u64 = <Test as Config>::TimeslicePeriod::get();
		advance_to(timeslice_period.saturating_mul(config.region_length.into()));
		assert_eq!(Leases::<Test>::get().len(), 0);
	});
}

#[test]
fn leases_can_be_renewed() {
	let initial_balance = 100_000;
	TestExt::new().endow(1, initial_balance).execute_with(|| {
		// Timeslice period is 2.
		//
		// Sale 1 starts at block 7, Sale 2 starts at 13.

		// Set lease to expire in sale 1 and start sales.
		assert_ok!(Broker::do_set_lease(2001, 9));
		assert_eq!(Leases::<Test>::get().len(), 1);
		// Start the sales with only one core for this lease.
		assert_ok!(Broker::do_start_sales(100, 0));

		// Advance to sale period 1, we should get an PotentialRenewal for task 2001 for the next
		// sale.
		advance_sale_period();
		assert_eq!(
			PotentialRenewals::<Test>::get(PotentialRenewalId { core: 0, when: 10 }),
			Some(PotentialRenewalRecord {
				price: 1000,
				completion: CompletionStatus::Complete(
					vec![ScheduleItem { mask: CoreMask::complete(), assignment: Task(2001) }]
						.try_into()
						.unwrap()
				)
			})
		);
		// And the lease has been removed from storage.
		assert_eq!(Leases::<Test>::get().len(), 0);

		// Advance to sale period 2, where we can renew.
		advance_sale_period();
		assert_ok!(Broker::do_renew(1, 0));
		// We renew for the price of the previous sale period.
		assert_eq!(balance(1), initial_balance - 1000);

		// We just renewed for this period.
		advance_sale_period();
		// Now we are off core and the core is pooled.
		advance_sale_period();
		// Check the trace agrees.
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				// Period 0 gets no assign core, but leases are on-core.
				// Period 1:
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(CoreAssignment::Task(2001), 57600)],
						end_hint: None,
					},
				),
				// Period 2 - expiring at the end of this period, so we called renew.
				(
					12,
					AssignCore {
						core: 0,
						begin: 14,
						assignment: vec![(CoreAssignment::Task(2001), 57600)],
						end_hint: None,
					},
				),
				// Period 3 - we get assigned a core because we called renew in period 2.
				(
					18,
					AssignCore {
						core: 0,
						begin: 20,
						assignment: vec![(CoreAssignment::Task(2001), 57600)],
						end_hint: None,
					},
				),
				// Period 4 - we don't get a core as we didn't call renew again.
				// This core is recycled into the pool.
				(
					24,
					AssignCore {
						core: 0,
						begin: 26,
						assignment: vec![(CoreAssignment::Pool, 57600)],
						end_hint: None,
					},
				),
			]
		);
	});
}

// We understand that this does not work as intended for leases that expire within `region_length`
// timeslices after calling `start_sales`.
#[test]
fn short_leases_cannot_be_renewed() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		// Timeslice period is 2.
		//
		// Sale 1 starts at block 7, Sale 2 starts at 13.

		// Set lease to expire in sale period 0 and start sales.
		assert_ok!(Broker::do_set_lease(2001, 3));
		assert_eq!(Leases::<Test>::get().len(), 1);
		// Start the sales with one core for this lease.
		assert_ok!(Broker::do_start_sales(100, 0));

		// The lease is removed.
		assert_eq!(Leases::<Test>::get().len(), 0);

		// We should have got an entry in PotentialRenewals, but we don't because rotate_sale
		// schedules leases a period in advance. This renewal should be in the period after next
		// because while bootstrapping our way into the sale periods, we give everything a lease for
		// period 1, so they can renew for period 2. So we have a core until the end of period 1,
		// but we are not marked as able to renew because we expired before sale period 1 starts.
		//
		// This should be fixed.
		assert_eq!(PotentialRenewals::<Test>::get(PotentialRenewalId { core: 0, when: 10 }), None);
		// And the lease has been removed from storage.
		assert_eq!(Leases::<Test>::get().len(), 0);

		// Advance to sale period 2, where we now cannot renew.
		advance_to(13);
		assert_noop!(Broker::do_renew(1, 0), Error::<Test>::NotAllowed);

		// Check the trace.
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				// Period 0 gets no assign core, but leases are on-core.
				// Period 1 we get assigned a core due to the way the sales are bootstrapped.
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(CoreAssignment::Task(2001), 57600)],
						end_hint: None,
					},
				),
				// Period 2 - we don't get a core as we couldn't renew.
				// This core is recycled into the pool.
				(
					12,
					AssignCore {
						core: 0,
						begin: 14,
						assignment: vec![(CoreAssignment::Pool, 57600)],
						end_hint: None,
					},
				),
			]
		);
	});
}

#[test]
fn leases_are_limited() {
	TestExt::new().execute_with(|| {
		let max_leases: u32 = <Test as Config>::MaxLeasedCores::get();
		Leases::<Test>::put(
			BoundedVec::try_from(vec![
				LeaseRecordItem { task: 1u32, until: 10u32 };
				max_leases as usize
			])
			.unwrap(),
		);
		assert_noop!(Broker::do_set_lease(1000, 10), Error::<Test>::TooManyLeases);
	});
}

#[test]
fn remove_lease_works() {
	TestExt::new().execute_with(|| {
		Leases::<Test>::put(
			BoundedVec::try_from(vec![LeaseRecordItem { task: 1u32, until: 10u32 }]).unwrap(),
		);
		assert_noop!(Broker::do_remove_lease(2), Error::<Test>::LeaseNotFound);
		assert_ok!(Broker::do_remove_lease(1));
		assert_noop!(Broker::do_remove_lease(1), Error::<Test>::LeaseNotFound);
	});
}

#[test]
fn purchase_requires_valid_status_and_sale_info() {
	TestExt::new().execute_with(|| {
		assert_noop!(Broker::do_purchase(1, 100), Error::<Test>::Uninitialized);

		let status = StatusRecord {
			core_count: 2,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: 0,
			last_timeslice: 1,
		};
		Status::<Test>::put(&status);
		assert_noop!(Broker::do_purchase(1, 100), Error::<Test>::NoSales);

		let mut dummy_sale = SaleInfoRecord {
			sale_start: 0,
			leadin_length: 0,
			end_price: 200,
			sellout_price: None,
			region_begin: 0,
			region_end: 3,
			first_core: 3,
			ideal_cores_sold: 0,
			cores_offered: 1,
			cores_sold: 2,
			sale_index: 0,
		};
		SaleInfo::<Test>::put(&dummy_sale);
		assert_noop!(Broker::do_purchase(1, 100), Error::<Test>::Unavailable);

		dummy_sale.first_core = 1;
		SaleInfo::<Test>::put(&dummy_sale);
		assert_noop!(Broker::do_purchase(1, 100), Error::<Test>::SoldOut);

		assert_ok!(Broker::do_start_sales(200, 1));
		assert_noop!(Broker::do_purchase(1, 100), Error::<Test>::TooEarly);

		advance_to(2);
		assert_noop!(Broker::do_purchase(1, 100), Error::<Test>::Overpriced);
	});
}

#[test]
fn renewal_requires_valid_status_and_sale_info() {
	TestExt::new().execute_with(|| {
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::Uninitialized);

		let status = StatusRecord {
			core_count: 2,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: 0,
			last_timeslice: 1,
		};
		Status::<Test>::put(&status);
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::NoSales);

		let mut dummy_sale = SaleInfoRecord {
			sale_start: 0,
			leadin_length: 0,
			end_price: 200,
			sellout_price: None,
			region_begin: 0,
			region_end: 3,
			first_core: 3,
			ideal_cores_sold: 0,
			cores_offered: 1,
			cores_sold: 2,
			sale_index: 0,
		};
		SaleInfo::<Test>::put(&dummy_sale);
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::Unavailable);

		dummy_sale.first_core = 1;
		SaleInfo::<Test>::put(&dummy_sale);
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::SoldOut);

		assert_ok!(Broker::do_start_sales(200, 1));
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::NotAllowed);

		let record = PotentialRenewalRecord {
			price: 100,
			completion: CompletionStatus::Partial(CoreMask::from_chunk(0, 20)),
		};
		PotentialRenewals::<Test>::insert(PotentialRenewalId { core: 1, when: 4 }, &record);
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::IncompleteAssignment);
	});
}

#[test]
fn cannot_transfer_or_partition_or_interlace_unknown() {
	TestExt::new().execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region_id = RegionId { begin: 0, core: 0, mask: CoreMask::complete() };
		assert_noop!(Broker::do_transfer(region_id, None, 2), Error::<Test>::UnknownRegion);
		assert_noop!(Broker::do_partition(region_id, None, 2), Error::<Test>::UnknownRegion);
		assert_noop!(
			Broker::do_interlace(region_id, None, CoreMask::from_chunk(0, 20)),
			Error::<Test>::UnknownRegion
		);
	});
}

#[test]
fn check_ownership_for_transfer_or_partition_or_interlace() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_noop!(Broker::do_transfer(region, Some(2), 2), Error::<Test>::NotOwner);
		assert_noop!(Broker::do_partition(region, Some(2), 2), Error::<Test>::NotOwner);
		assert_noop!(
			Broker::do_interlace(region, Some(2), CoreMask::from_chunk(0, 20)),
			Error::<Test>::NotOwner
		);
	});
}

#[test]
fn cannot_partition_invalid_offset() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_noop!(Broker::do_partition(region, None, 0), Error::<Test>::PivotTooEarly);
		assert_noop!(Broker::do_partition(region, None, 5), Error::<Test>::PivotTooLate);
	});
}

#[test]
fn cannot_interlace_invalid_pivot() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		let (region1, _) = Broker::do_interlace(region, None, CoreMask::from_chunk(0, 20)).unwrap();
		assert_noop!(
			Broker::do_interlace(region1, None, CoreMask::from_chunk(20, 40)),
			Error::<Test>::ExteriorPivot
		);
		assert_noop!(
			Broker::do_interlace(region1, None, CoreMask::void()),
			Error::<Test>::VoidPivot
		);
		assert_noop!(
			Broker::do_interlace(region1, None, CoreMask::from_chunk(0, 20)),
			Error::<Test>::CompletePivot
		);
	});
}

#[test]
fn assign_should_drop_invalid_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let mut region = Broker::do_purchase(1, u64::max_value()).unwrap();
		advance_to(10);
		assert_ok!(Broker::do_assign(region, Some(1), 1001, Provisional));
		region.begin = 7;
		System::assert_last_event(Event::RegionDropped { region_id: region, duration: 3 }.into());
	});
}

#[test]
fn pool_should_drop_invalid_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let mut region = Broker::do_purchase(1, u64::max_value()).unwrap();
		advance_to(10);
		assert_ok!(Broker::do_pool(region, Some(1), 1001, Provisional));
		region.begin = 7;
		System::assert_last_event(Event::RegionDropped { region_id: region, duration: 3 }.into());
	});
}

#[test]
fn config_works() {
	TestExt::new().execute_with(|| {
		let mut cfg = new_config();
		// Good config works:
		assert_ok!(Broker::configure(Root.into(), cfg.clone()));
		// Bad config is a noop:
		cfg.leadin_length = 0;
		assert_noop!(Broker::configure(Root.into(), cfg), Error::<Test>::InvalidConfig);
	});
}

/// Ensure that a lease that ended before `start_sales` was called can be renewed.
#[test]
fn renewal_works_leases_ended_before_start_sales() {
	TestExt::new().endow(1, 100_000).execute_with(|| {
		let config = Configuration::<Test>::get().unwrap();

		// This lease is ended before `start_stales` was called.
		assert_ok!(Broker::do_set_lease(1, 1));

		// Go to some block to ensure that the lease of task 1 already ended.
		advance_to(5);

		// This lease will end three sale periods in.
		assert_ok!(Broker::do_set_lease(
			2,
			Broker::latest_timeslice_ready_to_commit(&config) + config.region_length * 3
		));

		// This intializes the first sale and the period 0.
		assert_ok!(Broker::do_start_sales(100, 0));
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::Unavailable);
		assert_noop!(Broker::do_renew(1, 0), Error::<Test>::Unavailable);

		// Lease for task 1 should have been dropped.
		assert!(Leases::<Test>::get().iter().any(|l| l.task == 2));

		// This intializes the second and the period 1.
		advance_sale_period();

		// Now we can finally renew the core 0 of task 1.
		let new_core = Broker::do_renew(1, 0).unwrap();
		// Renewing the active lease doesn't work.
		assert_noop!(Broker::do_renew(1, 1), Error::<Test>::SoldOut);
		assert_eq!(balance(1), 99000);

		// This intializes the third sale and the period 2.
		advance_sale_period();
		let new_core = Broker::do_renew(1, new_core).unwrap();

		// Renewing the active lease doesn't work.
		assert_noop!(Broker::do_renew(1, 0), Error::<Test>::SoldOut);
		assert_eq!(balance(1), 98900);

		// All leases should have ended
		assert!(Leases::<Test>::get().is_empty());

		// This intializes the fourth sale and the period 3.
		advance_sale_period();

		// Renew again
		assert_eq!(0, Broker::do_renew(1, new_core).unwrap());
		// Renew the task 2.
		assert_eq!(1, Broker::do_renew(1, 0).unwrap());
		assert_eq!(balance(1), 98790);

		// This intializes the fifth sale and the period 4.
		advance_sale_period();

		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					10,
					AssignCore {
						core: 0,
						begin: 12,
						assignment: vec![(Task(1), 57600)],
						end_hint: None
					}
				),
				(
					10,
					AssignCore {
						core: 1,
						begin: 12,
						assignment: vec![(Task(2), 57600)],
						end_hint: None
					}
				),
				(
					16,
					AssignCore {
						core: 0,
						begin: 18,
						assignment: vec![(Task(2), 57600)],
						end_hint: None
					}
				),
				(
					16,
					AssignCore {
						core: 1,
						begin: 18,
						assignment: vec![(Task(1), 57600)],
						end_hint: None
					}
				),
				(
					22,
					AssignCore {
						core: 0,
						begin: 24,
						assignment: vec![(Task(2), 57600)],
						end_hint: None,
					},
				),
				(
					22,
					AssignCore {
						core: 1,
						begin: 24,
						assignment: vec![(Task(1), 57600)],
						end_hint: None,
					},
				),
				(
					28,
					AssignCore {
						core: 0,
						begin: 30,
						assignment: vec![(Task(1), 57600)],
						end_hint: None,
					},
				),
				(
					28,
					AssignCore {
						core: 1,
						begin: 30,
						assignment: vec![(Task(2), 57600)],
						end_hint: None,
					},
				),
			]
		);
	});
}

#[test]
fn enable_auto_renew_works() {
	TestExt::new().endow(1, 1000).limit_cores_offered(Some(10)).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 5));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		let record = Regions::<Test>::get(region_id).unwrap();

		// Cannot enable auto renewal with provisional finality:
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Provisional));
		assert_noop!(
			Broker::do_enable_auto_renew(1001, region_id.core, 1001, Some(7)),
			Error::<Test>::NotAllowed
		);

		// Eligible for renewal after final assignment:
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));
		assert!(PotentialRenewals::<Test>::get(PotentialRenewalId {
			core: region_id.core,
			when: record.end
		})
		.is_some());

		// Only the task's sovereign account can enable auto renewal.
		assert_noop!(
			Broker::enable_auto_renew(RuntimeOrigin::signed(1), region_id.core, 1001, Some(7)),
			Error::<Test>::NoPermission
		);

		// Works when calling with the sovereign account:
		assert_ok!(Broker::do_enable_auto_renew(1001, region_id.core, 1001, Some(7)));
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord { core: 0, task: 1001, next_renewal: 7 }]
		);
		System::assert_has_event(
			Event::<Test>::AutoRenewalEnabled { core: region_id.core, task: 1001 }.into(),
		);

		// Enabling auto-renewal for more cores to ensure they are sorted based on core index.
		let region_2 = Broker::do_purchase(1, u64::max_value()).unwrap();
		let region_3 = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_2, Some(1), 1002, Final));
		assert_ok!(Broker::do_assign(region_3, Some(1), 1003, Final));
		assert_ok!(Broker::do_enable_auto_renew(1003, region_3.core, 1003, Some(7)));
		assert_ok!(Broker::do_enable_auto_renew(1002, region_2.core, 1002, Some(7)));

		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![
				AutoRenewalRecord { core: 0, task: 1001, next_renewal: 7 },
				AutoRenewalRecord { core: 1, task: 1002, next_renewal: 7 },
				AutoRenewalRecord { core: 2, task: 1003, next_renewal: 7 },
			]
		);

		// Ensure that we cannot enable more auto renewals than `MaxAutoRenewals`.
		// We already enabled it for three cores, and the limit is set to 3.
		let region_4 = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_4, Some(1), 1004, Final));

		assert_noop!(
			Broker::do_enable_auto_renew(1004, region_4.core, 1004, Some(7)),
			Error::<Test>::TooManyAutoRenewals
		);
	});
}

#[test]
fn enable_auto_renewal_works_for_legacy_leases() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		// With this test, we ensure that we don't renew unnecessarily if the task has Coretime
		// reserved (due to having a lease)

		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);

		let record = PotentialRenewalRecord {
			price: 100,
			completion: CompletionStatus::Complete(
				vec![ScheduleItem { mask: CoreMask::complete(), assignment: Task(1001) }]
					.try_into()
					.unwrap(),
			),
		};
		// For lease holding tasks, the renewal record is set for when the lease expires, which is
		// likely further in the future than the start of the next sale.
		PotentialRenewals::<Test>::insert(PotentialRenewalId { core: 0, when: 10 }, &record);

		endow(1001, 1000);

		// Will fail if we don't provide the end hint since it expects renewal record to be at next
		// sale start.
		assert_noop!(
			Broker::do_enable_auto_renew(1001, 0, 1001, None),
			Error::<Test>::TaskNotInWorkload
		);

		assert_ok!(Broker::do_enable_auto_renew(1001, 0, 1001, Some(10)));
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord { core: 0, task: 1001, next_renewal: 10 },]
		);
		System::assert_has_event(Event::<Test>::AutoRenewalEnabled { core: 0, task: 1001 }.into());

		// Next cycle starting at 7.
		advance_to(7);

		// Ensure that the renewal didn't happen by checking that the balance remained the same, as
		// there is still no need to renew.
		assert_eq!(balance(1001), 1000);

		// The next sale starts at 13. The renewal should happen now and the account should be
		// charged.
		advance_to(13);
		assert_eq!(balance(1001), 900);

		// Make sure that the renewal happened:
		System::assert_has_event(
			Event::<Test>::Renewed {
				who: 1001, // sovereign account
				old_core: 0,
				core: 0,
				price: 100,
				begin: 10,
				duration: 3,
				workload: Schedule::truncate_from(vec![ScheduleItem {
					assignment: Task(1001),
					mask: CoreMask::complete(),
				}]),
			}
			.into(),
		);
	});
}

#[test]
fn enable_auto_renew_renews() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();

		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));
		// advance to next bulk sale:
		advance_to(6);

		// Since we didn't renew for the next bulk period, enabling auto-renewal will renew,
		// ensuring the task continues execution.

		// Will fail because we didn't fund the sovereign account:
		assert_noop!(
			Broker::do_enable_auto_renew(1001, region_id.core, 1001, None),
			TokenError::FundsUnavailable
		);

		// Will succeed after funding the sovereign account:
		endow(1001, 1000);

		assert_ok!(Broker::do_enable_auto_renew(1001, region_id.core, 1001, None));
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord { core: 0, task: 1001, next_renewal: 10 }]
		);
		assert!(PotentialRenewals::<Test>::get(PotentialRenewalId {
			core: region_id.core,
			when: 10
		})
		.is_some());

		System::assert_has_event(
			Event::<Test>::AutoRenewalEnabled { core: region_id.core, task: 1001 }.into(),
		);
	});
}

#[test]
fn auto_renewal_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 3));
		advance_to(2);
		let region_1 = Broker::do_purchase(1, u64::max_value()).unwrap();
		let region_2 = Broker::do_purchase(1, u64::max_value()).unwrap();
		let region_3 = Broker::do_purchase(1, u64::max_value()).unwrap();

		// Eligible for renewal after final assignment:
		assert_ok!(Broker::do_assign(region_1, Some(1), 1001, Final));
		assert_ok!(Broker::do_assign(region_2, Some(1), 1002, Final));
		assert_ok!(Broker::do_assign(region_3, Some(1), 1003, Final));
		assert_ok!(Broker::do_enable_auto_renew(1001, region_1.core, 1001, Some(7)));
		assert_ok!(Broker::do_enable_auto_renew(1002, region_2.core, 1002, Some(7)));
		assert_ok!(Broker::do_enable_auto_renew(1003, region_3.core, 1003, Some(7)));
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![
				AutoRenewalRecord { core: 0, task: 1001, next_renewal: 7 },
				AutoRenewalRecord { core: 1, task: 1002, next_renewal: 7 },
				AutoRenewalRecord { core: 2, task: 1003, next_renewal: 7 },
			]
		);

		// We have to fund the sovereign account:
		endow(1001, 1000);
		// We skip funding the sovereign account of task 1002 on purpose.
		endow(1003, 1000);

		// Next cycle starting at 7.
		advance_to(7);
		System::assert_has_event(
			Event::<Test>::Renewed {
				who: 1001, // sovereign account
				old_core: 0,
				core: 0,
				price: 100,
				begin: 7,
				duration: 3,
				workload: Schedule::truncate_from(vec![ScheduleItem {
					assignment: Task(1001),
					mask: CoreMask::complete(),
				}]),
			}
			.into(),
		);
		// Sovereign account wasn't funded so it fails:
		System::assert_has_event(
			Event::<Test>::AutoRenewalFailed { core: 1, payer: Some(1002) }.into(),
		);
		System::assert_has_event(
			Event::<Test>::Renewed {
				who: 1003, // sovereign account
				old_core: 2,
				core: 1, // Core #1 didn't get renewed, so core #2 will take its place.
				price: 100,
				begin: 7,
				duration: 3,
				workload: Schedule::truncate_from(vec![ScheduleItem {
					assignment: Task(1003),
					mask: CoreMask::complete(),
				}]),
			}
			.into(),
		);

		// Given that core #1 didn't get renewed due to the account not being sufficiently funded,
		// Task (1003) will now be assigned to that core instead of core #2.
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![
				AutoRenewalRecord { core: 0, task: 1001, next_renewal: 10 },
				AutoRenewalRecord { core: 1, task: 1003, next_renewal: 10 },
			]
		);
	});
}

#[test]
fn enable_auto_renew_immediate_updates_core_and_renews() {
	TestExt::new().endow(1, 1000).endow(2, 1000).endow(1001, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));

		// Rotate into the next sale where this region is renewable.
		let sale = SaleInfo::<Test>::get().unwrap();
		let timeslice_period: u64 = <Test as Config>::TimeslicePeriod::get();
		let next_sale_block = sale.region_begin as u64 * timeslice_period;
		advance_to(next_sale_block);

		let sale = SaleInfo::<Test>::get().unwrap();
		if System::block_number() <= sale.sale_start {
			advance_to(sale.sale_start + 1);
		}

		// Pre-sell a core to ensure the renewal allocates a different core index.
		let _ = Broker::do_purchase(2, u64::max_value()).unwrap();
		let sale_before_renew = SaleInfo::<Test>::get().unwrap();
		let expected_new_core = sale_before_renew.first_core + sale_before_renew.cores_sold;
		assert_ne!(expected_new_core, region_id.core);

		assert_ok!(Broker::do_enable_auto_renew(1001, region_id.core, 1001, None));

		// Auto-renewal record should follow the new core.
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord {
				core: expected_new_core,
				task: 1001,
				next_renewal: sale_before_renew.region_end
			}]
		);

		// Potential renewal moved to the new core index.
		assert!(PotentialRenewals::<Test>::get(PotentialRenewalId {
			core: expected_new_core,
			when: sale_before_renew.region_end
		})
		.is_some());
		assert!(PotentialRenewals::<Test>::get(PotentialRenewalId {
			core: region_id.core,
			when: sale_before_renew.region_end
		})
		.is_none());

		// Next rotation should renew again and keep auto-renewal enabled.
		let next_block = sale_before_renew.region_end as u64 * timeslice_period;
		advance_to(next_block);
		let sale_after_renew = SaleInfo::<Test>::get().unwrap();
		let auto_after_renew = AutoRenewals::<Test>::get().to_vec();
		assert_eq!(auto_after_renew.len(), 1);
		assert_eq!(auto_after_renew[0].task, 1001);
		assert_eq!(auto_after_renew[0].next_renewal, sale_after_renew.region_end);
		assert!(PotentialRenewals::<Test>::get(PotentialRenewalId {
			core: auto_after_renew[0].core,
			when: sale_after_renew.region_end
		})
		.is_some());
	});
}

#[test]
fn enable_auto_renew_fails_for_workload_of_another_task() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);
		let region_1 = Broker::do_purchase(1, u64::max_value()).unwrap();
		let region_2 = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_1, Some(1), 1001, Final));
		assert_ok!(Broker::do_assign(region_2, Some(1), 2001, Final));
		endow(1001, 1000);

		// Task 1001 cannot enable auto-renewal for the core running task 2001's workload, even
		// with a valid workload end hint:
		assert_noop!(
			Broker::do_enable_auto_renew(1001, region_2.core, 1001, Some(7)),
			Error::<Test>::TaskNotInWorkload
		);

		// Now both cores are expiring, so enabling auto-renewal would renew immediately. Task
		// 1001 must not renew (and pay for) task 2001's workload:
		advance_to(6);
		assert_noop!(
			Broker::do_enable_auto_renew(1001, region_2.core, 1001, None),
			Error::<Test>::TaskNotInWorkload
		);
		assert_eq!(balance(1001), 1000);
		assert_eq!(AutoRenewals::<Test>::get().to_vec(), vec![]);

		// Works for the core running its own workload:
		assert_ok!(Broker::do_enable_auto_renew(1001, region_1.core, 1001, None));
		assert_eq!(balance(1001), 900);
	});
}

#[test]
fn enable_auto_renew_falls_back_to_hint_when_core_carries_foreign_renewal() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));
		endow(2001, 1000);

		// The core is expiring with task 1001's workload, while task 2001 has its own renewal
		// record on the same core index further in the future:
		advance_to(6);
		let sale = SaleInfo::<Test>::get().unwrap();
		PotentialRenewals::<Test>::insert(
			PotentialRenewalId { core: region_id.core, when: sale.region_end },
			PotentialRenewalRecord {
				price: 100,
				completion: CompletionStatus::Complete(
					vec![ScheduleItem { mask: CoreMask::complete(), assignment: Task(2001) }]
						.try_into()
						.unwrap(),
				),
			},
		);

		// Task 2001 must not renew task 1001's expiring workload; the hint leads to its own
		// renewal record instead:
		assert_ok!(Broker::do_enable_auto_renew(2001, region_id.core, 2001, Some(sale.region_end)));
		assert_eq!(balance(2001), 1000);
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord {
				core: region_id.core,
				task: 2001,
				next_renewal: sale.region_end
			}]
		);
		// Task 1001's expiring workload was left untouched:
		assert!(PotentialRenewals::<Test>::get(PotentialRenewalId {
			core: region_id.core,
			when: sale.region_begin
		})
		.is_some());
	});
}

#[test]
fn enable_auto_renew_fails_for_incomplete_workload() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		endow(1001, 1000);

		// A partially assigned renewal record cannot be renewed, even by a task which is part
		// of the partial workload:
		PotentialRenewals::<Test>::insert(
			PotentialRenewalId { core: 0, when: 10 },
			PotentialRenewalRecord {
				price: 100,
				completion: CompletionStatus::Partial(CoreMask::from_chunk(0, 40)),
			},
		);
		assert_noop!(
			Broker::do_enable_auto_renew(1001, 0, 1001, Some(10)),
			Error::<Test>::IncompleteAssignment
		);
	});
}

#[test]
fn auto_renewal_fails_for_workload_of_another_task() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));
		assert_ok!(Broker::do_enable_auto_renew(1001, region_id.core, 1001, Some(7)));
		endow(1001, 1000);

		// The workload on the core changes hands before the renewal is processed (e.g. a stale
		// record created before the task-workload check existed):
		let renewal_id = PotentialRenewalId { core: region_id.core, when: 7 };
		let record = PotentialRenewals::<Test>::get(renewal_id).unwrap();
		PotentialRenewals::<Test>::insert(
			renewal_id,
			PotentialRenewalRecord {
				price: record.price,
				completion: CompletionStatus::Complete(
					vec![ScheduleItem { mask: CoreMask::complete(), assignment: Task(2001) }]
						.try_into()
						.unwrap(),
				),
			},
		);

		// The renewal fails instead of charging task 1001 for task 2001's workload:
		advance_to(7);
		assert_eq!(balance(1001), 1000);
		System::assert_has_event(
			Event::<Test>::AutoRenewalFailed { core: region_id.core, payer: Some(1001) }.into(),
		);
		// The mismatched record is dropped:
		assert_eq!(AutoRenewals::<Test>::get().to_vec(), vec![]);
	});
}

#[test]
fn auto_renewal_skips_record_with_future_next_renewal() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);

		// Task 1001 has a renewal record further in the future (e.g. due to holding a lease)
		// and enables auto-renewal with a hint:
		PotentialRenewals::<Test>::insert(
			PotentialRenewalId { core: 0, when: 10 },
			PotentialRenewalRecord {
				price: 100,
				completion: CompletionStatus::Complete(
					vec![ScheduleItem { mask: CoreMask::complete(), assignment: Task(1001) }]
						.try_into()
						.unwrap(),
				),
			},
		);
		endow(1001, 1000);
		assert_ok!(Broker::do_enable_auto_renew(1001, 0, 1001, Some(10)));

		// In the meantime, the same core index carries another task's expiring workload:
		PotentialRenewals::<Test>::insert(
			PotentialRenewalId { core: 0, when: 7 },
			PotentialRenewalRecord {
				price: 100,
				completion: CompletionStatus::Complete(
					vec![ScheduleItem { mask: CoreMask::complete(), assignment: Task(2001) }]
						.try_into()
						.unwrap(),
				),
			},
		);

		// The record is not due for renewal yet, so it must be skipped and left untouched
		// rather than dropped because of the foreign workload:
		advance_to(7);
		assert_eq!(balance(1001), 1000);
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord { core: 0, task: 1001, next_renewal: 10 }]
		);

		// Once due, the renewal is processed normally:
		advance_to(13);
		assert_eq!(balance(1001), 900);
	});
}

#[test]
fn enable_auto_renew_immediate_ignores_hint_for_next_renewal() {
	TestExt::new().endow(1, 1000).endow(1001, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));
		advance_to(6);

		// The core is renewable now, so enabling auto-renewal renews immediately. The hint must
		// not delay the next renewal past the period just renewed for:
		assert_ok!(Broker::do_enable_auto_renew(1001, region_id.core, 1001, Some(100)));
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord { core: 0, task: 1001, next_renewal: 10 }]
		);
	});
}

#[test]
fn disable_auto_renew_works() {
	TestExt::new().endow(1, 1000).limit_cores_offered(Some(10)).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 3));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();

		// Eligible for renewal after final assignment:
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));

		// Cannot disable auto-renewal if we don't have it enabled.
		assert_noop!(
			Broker::do_disable_auto_renew(region_id.core, 1001),
			Error::<Test>::AutoRenewalNotEnabled
		);

		assert_ok!(Broker::do_enable_auto_renew(1001, region_id.core, 1001, Some(7)));
		assert_eq!(
			AutoRenewals::<Test>::get().to_vec(),
			vec![AutoRenewalRecord { core: 0, task: 1001, next_renewal: 7 }]
		);

		// Only the sovereign account can disable:
		assert_noop!(
			Broker::disable_auto_renew(RuntimeOrigin::signed(1), 0, 1001),
			Error::<Test>::NoPermission
		);
		assert_ok!(Broker::do_disable_auto_renew(0, 1001));

		assert_eq!(AutoRenewals::<Test>::get().to_vec(), vec![]);
		System::assert_has_event(
			Event::<Test>::AutoRenewalDisabled { core: region_id.core, task: 1001 }.into(),
		);
	});
}

#[test]
fn remove_assignment_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 1));
		advance_to(2);
		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region_id, Some(1), 1001, Final));
		let workplan_key = (region_id.begin, region_id.core);
		assert_ne!(Workplan::<Test>::get(workplan_key), None);
		assert_noop!(Broker::remove_assignment(RuntimeOrigin::signed(2), region_id), BadOrigin);
		assert_ok!(Broker::remove_assignment(RuntimeOrigin::root(), region_id));
		assert_eq!(Workplan::<Test>::get(workplan_key), None);
		assert_noop!(
			Broker::remove_assignment(RuntimeOrigin::root(), region_id),
			Error::<Test>::AssignmentNotFound
		);
	});
}

#[test]
fn start_sales_sets_correct_core_count() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		advance_to(1);

		Broker::do_set_lease(1, 100).unwrap();
		Broker::do_set_lease(2, 100).unwrap();
		Broker::do_set_lease(3, 100).unwrap();
		Broker::do_reserve(Schedule::truncate_from(vec![ScheduleItem {
			assignment: Pool,
			mask: CoreMask::complete(),
		}]))
		.unwrap();

		Broker::do_start_sales(5, 5).unwrap();

		System::assert_has_event(Event::<Test>::CoreCountRequested { core_count: 9 }.into());
	})
}

// Reservations currently need two sale period boundaries to pass before coming into effect.
#[test]
fn reserve_works() {
	TestExt::new().execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 0));
		// Advance forward from start_sales, but not into the first sale.
		advance_to(1);

		let system_workload = Schedule::truncate_from(vec![ScheduleItem {
			mask: CoreMask::complete(),
			assignment: Task(1004),
		}]);

		// This shouldn't work, as the reservation will never be assigned a core unless one is
		// available.
		// assert_noop!(Broker::do_reserve(system_workload.clone()), Error::<Test>::Unavailable);

		// Add another core and create the reservation.
		let status = Status::<Test>::get().unwrap();
		assert_ok!(Broker::request_core_count(RuntimeOrigin::root(), status.core_count + 1));
		assert_ok!(Broker::reserve(RuntimeOrigin::root(), system_workload.clone()));

		// This is added to reservations.
		System::assert_last_event(
			Event::ReservationMade { index: 0, workload: system_workload.clone() }.into(),
		);
		assert_eq!(Reservations::<Test>::get(), vec![system_workload.clone()]);

		// But not yet in workplan for any of the next few regions.
		for i in 0..20 {
			assert_eq!(Workplan::<Test>::get((i, 0)), None);
		}
		// And it hasn't been assigned a core.
		assert_eq!(CoretimeTrace::get(), vec![]);

		// Go to next sale. Rotate sale puts it in the workplan.
		advance_sale_period();
		assert_eq!(Workplan::<Test>::get((7, 0)), Some(system_workload.clone()));
		// But it still hasn't been assigned a core.
		assert_eq!(CoretimeTrace::get(), vec![]);

		// Go to the second sale after reserving.
		advance_sale_period();
		// Core is assigned at block 14 (timeslice 7) after being reserved all the way back at
		// timeslice 1! Since the mock periods are 3 timeslices long, this means that reservations
		// made in period 0 will only come into effect in period 2.
		assert_eq!(
			CoretimeTrace::get(),
			vec![(
				12,
				AssignCore {
					core: 0,
					begin: 14,
					assignment: vec![(Task(1004), 57600)],
					end_hint: None
				}
			)]
		);
		System::assert_has_event(
			Event::CoreAssigned {
				core: 0,
				when: 14,
				assignment: vec![(CoreAssignment::Task(1004), 57600)],
			}
			.into(),
		);

		// And it's in the workplan for the next period.
		assert_eq!(Workplan::<Test>::get((10, 0)), Some(system_workload.clone()));
	});
}

// We can use a hack to accelerate this by injecting it into the workplan.
#[test]
fn can_reserve_workloads_quickly() {
	TestExt::new().execute_with(|| {
		// Start sales.
		assert_ok!(Broker::do_start_sales(100, 0));
		advance_to(2);

		let system_workload = Schedule::truncate_from(vec![ScheduleItem {
			mask: CoreMask::complete(),
			assignment: Task(1004),
		}]);

		// This shouldn't work, as the reservation will never be assigned a core unless one is
		// available.
		// assert_noop!(Broker::do_reserve(system_workload.clone()), Error::<Test>::Unavailable);

		// Add another core and create the reservation.
		let core_count = Status::<Test>::get().unwrap().core_count;
		assert_ok!(Broker::request_core_count(RuntimeOrigin::root(), core_count + 1));
		assert_ok!(Broker::reserve(RuntimeOrigin::root(), system_workload.clone()));

		// These are the additional steps to onboard this immediately.
		let core_index = core_count;
		// In a real network this would call the relay chain
		// `assigner_coretime::assign_core` extrinsic directly.
		<TestCoretimeProvider as CoretimeInterface>::assign_core(
			core_index,
			2,
			vec![(Task(1004), 57600)],
			None,
		);
		// Inject into the workplan to ensure it's scheduled in the next rotate_sale.
		Workplan::<Test>::insert((4, core_index), system_workload.clone());

		// Reservation is added for the workload.
		System::assert_has_event(
			Event::ReservationMade { index: 0, workload: system_workload.clone() }.into(),
		);
		System::assert_has_event(Event::CoreCountRequested { core_count: 1 }.into());

		// It is also in the workplan for the next region.
		assert_eq!(Workplan::<Test>::get((4, 0)), Some(system_workload.clone()));

		// Go to next sale. Rotate sale puts it in the workplan.
		advance_sale_period();
		assert_eq!(Workplan::<Test>::get((7, 0)), Some(system_workload.clone()));

		// Go to the second sale after reserving.
		advance_sale_period();

		// Check the trace to ensure it has a core in every region.
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				(
					2,
					AssignCore {
						core: 0,
						begin: 2,
						assignment: vec![(Task(1004), 57600)],
						end_hint: None
					}
				),
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Task(1004), 57600)],
						end_hint: None
					}
				),
				(
					12,
					AssignCore {
						core: 0,
						begin: 14,
						assignment: vec![(Task(1004), 57600)],
						end_hint: None
					}
				)
			]
		);
		System::assert_has_event(
			Event::CoreAssigned {
				core: 0,
				when: 8,
				assignment: vec![(CoreAssignment::Task(1004), 57600)],
			}
			.into(),
		);
		System::assert_has_event(
			Event::CoreAssigned {
				core: 0,
				when: 14,
				assignment: vec![(CoreAssignment::Task(1004), 57600)],
			}
			.into(),
		);
		System::assert_has_event(
			Event::CoreAssigned {
				core: 0,
				when: 14,
				assignment: vec![(CoreAssignment::Task(1004), 57600)],
			}
			.into(),
		);

		// And it's in the workplan for the next period.
		assert_eq!(Workplan::<Test>::get((10, 0)), Some(system_workload.clone()));
	});
}

#[test]
fn force_reserve_works() {
	let system_workload = Schedule::truncate_from(vec![ScheduleItem {
		mask: CoreMask::complete(),
		assignment: Task(1004),
	}]);

	// Not intended to work before sales are started.
	TestExt::new().execute_with(|| {
		assert_noop!(
			Broker::force_reserve(RuntimeOrigin::root(), system_workload.clone(), 0),
			Error::<Test>::NoSales
		);
	});

	// With active reservation and purchased coretime - ForceReservation should not overwrite.
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 4));
		advance_to(2);

		let existing_reservation = Schedule::truncate_from(vec![ScheduleItem {
			mask: CoreMask::complete(),
			assignment: Task(1000),
		}]);
		assert_ok!(Broker::reserve(RuntimeOrigin::root(), existing_reservation.clone()));

		// Advance 2 sale periods so the reservation becomes active.
		advance_sale_period();
		advance_sale_period();

		let region = Broker::do_purchase(1, u64::max_value()).unwrap();
		assert_ok!(Broker::do_assign(region, None, 1001, Final));

		assert_ok!(Broker::force_reserve(RuntimeOrigin::root(), system_workload.clone(), 3));

		assert_eq!(
			Reservations::<Test>::get(),
			vec![existing_reservation.clone(), system_workload.clone()]
		);
		assert_eq!(ForceReservations::<Test>::get(), vec![system_workload.clone()]);

		advance_sale_period();
		assert!(ForceReservations::<Test>::get().is_empty());

		advance_sale_period();

		// Trace shows:
		// - Existing reservation active at core 0 (from second sale period)
		// - Purchased coretime at core 1 (first_core = 1 after reservation)
		// - ForceReservation at core 2 (first free core after purchase)
		assert_eq!(
			CoretimeTrace::get(),
			vec![
				// First sale period: all cores to pool (reservation not yet active)
				(
					6,
					AssignCore {
						core: 0,
						begin: 8,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				(
					6,
					AssignCore {
						core: 1,
						begin: 8,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				(
					6,
					AssignCore {
						core: 2,
						begin: 8,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				(
					6,
					AssignCore {
						core: 3,
						begin: 8,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				// Second sale period: reservation becomes active at core 0
				(
					12,
					AssignCore {
						core: 0,
						begin: 14,
						assignment: vec![(Task(1000), 57600)],
						end_hint: None
					}
				),
				(
					12,
					AssignCore {
						core: 1,
						begin: 14,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				(
					12,
					AssignCore {
						core: 2,
						begin: 14,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				(
					12,
					AssignCore {
						core: 3,
						begin: 14,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				// Immediate assignment from force_reserve
				(
					16,
					AssignCore {
						core: 3,
						begin: 18,
						assignment: vec![(Task(1004), 57600)],
						end_hint: None
					}
				),
				// Third sale: reservation core 0, purchase core 1, ForceReservation core 2
				(
					18,
					AssignCore {
						core: 0,
						begin: 20,
						assignment: vec![(Task(1000), 57600)],
						end_hint: None
					}
				),
				(
					18,
					AssignCore {
						core: 1,
						begin: 20,
						assignment: vec![(Task(1001), 57600)],
						end_hint: None
					}
				),
				(
					18,
					AssignCore {
						core: 2,
						begin: 20,
						assignment: vec![(Task(1004), 57600)],
						end_hint: None
					}
				),
				(
					18,
					AssignCore {
						core: 3,
						begin: 20,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				// Fourth sale: both permanent reservations active
				(
					24,
					AssignCore {
						core: 0,
						begin: 26,
						assignment: vec![(Task(1000), 57600)],
						end_hint: None
					}
				),
				(
					24,
					AssignCore {
						core: 1,
						begin: 26,
						assignment: vec![(Task(1004), 57600)],
						end_hint: None
					}
				),
				(
					24,
					AssignCore {
						core: 2,
						begin: 26,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
				(
					24,
					AssignCore {
						core: 3,
						begin: 26,
						assignment: vec![(Pool, 57600)],
						end_hint: None
					}
				),
			]
		);
	});
}

#[test]
fn remove_potential_renewal_rejects_non_admin_origin() {
	TestExt::new().execute_with(|| {
		assert_noop!(
			Broker::remove_potential_renewal(RuntimeOrigin::signed(100), 0, 0),
			DispatchError::BadOrigin
		);

		assert_noop!(
			Broker::remove_potential_renewal(RuntimeOrigin::none(), 0, 0),
			DispatchError::BadOrigin
		);
	});
}

#[test]
fn remove_potential_renewal_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);

		assert_eq!(PotentialRenewals::<Test>::iter().count(), 0);

		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		let region = Regions::<Test>::get(region_id).unwrap();
		const TASK_ID: TaskId = 1111;
		Broker::do_assign(region_id, None, TASK_ID, Finality::Final).unwrap();

		// When assigning task to the region it's expected that the potential renewal with the
		// `when` field equal to the `region.end` will appear if the assignment is final.
		assert_eq!(PotentialRenewals::<Test>::iter().count(), 1);

		assert_noop!(
			Broker::do_remove_potential_renewal(region_id.core + 1, region.end),
			Error::<Test>::UnknownRenewal
		);

		assert_noop!(
			Broker::do_remove_potential_renewal(region_id.core, region.end + 1),
			Error::<Test>::UnknownRenewal
		);

		let new_region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		let new_region = Regions::<Test>::get(new_region_id).unwrap();
		const NEW_TASK_ID: TaskId = 2222;
		Broker::do_assign(new_region_id, None, NEW_TASK_ID, Finality::Final).unwrap();

		assert_eq!(PotentialRenewals::<Test>::iter().count(), 2);

		// Check that target renewal is not expired, so ensure that `do_remove_potential_renewal`
		// can remove non-expired renewals too.
		assert_err!(Broker::do_drop_renewal(region_id.core, region.end), Error::<Test>::StillValid);

		Broker::do_remove_potential_renewal(region_id.core, region.end).unwrap();
		System::assert_has_event(
			Event::PotentialRenewalRemoved { core: region_id.core, timeslice: region.end }.into(),
		);

		assert_eq!(PotentialRenewals::<Test>::iter().count(), 1);
		// Ensure that the correct potential renewal was removed.
		let renewal_ids: Vec<_> = PotentialRenewals::<Test>::iter().map(|(id, _)| id).collect();
		assert_eq!(
			renewal_ids,
			vec![PotentialRenewalId { core: new_region_id.core, when: new_region.end }]
		);

		assert_noop!(
			Broker::do_remove_potential_renewal(region_id.core, region.end),
			Error::<Test>::UnknownRenewal
		);
	});
}

#[test]
fn remove_potential_renewal_makes_auto_renewal_die() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 2));
		advance_to(2);

		let region_id = Broker::do_purchase(1, u64::max_value()).unwrap();
		const TASK_ID: TaskId = 1111;
		Broker::do_assign(region_id, None, TASK_ID, Finality::Final).unwrap();

		advance_to(6);

		Broker::do_enable_auto_renew(1, region_id.core, TASK_ID, None).unwrap();

		assert_eq!(AutoRenewals::<Test>::get().len(), 1);

		let renewals: Vec<_> = PotentialRenewals::<Test>::iter_keys().collect();
		assert_eq!(renewals.len(), 1);
		assert_ok!(Broker::do_remove_potential_renewal(renewals[0].core, renewals[0].when));

		assert_eq!(AutoRenewals::<Test>::get().len(), 1);

		advance_to(12);

		assert_eq!(AutoRenewals::<Test>::get().len(), 0);
	})
}

#[test]
fn force_transfer_works() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 4));
		advance_to(2);

		const OLD_OWNER: u64 = 1;
		const NEW_OWNER: u64 = 222;

		let region_id = Broker::do_purchase(OLD_OWNER, u64::max_value()).unwrap();
		let region = Regions::<Test>::get(region_id).unwrap();

		assert_noop!(
			Broker::force_transfer(
				RuntimeOrigin::root(),
				RegionId {
					begin: u32::max_value(),
					core: u16::max_value(),
					mask: CoreMask::void()
				},
				NEW_OWNER
			),
			Error::<Test>::UnknownRegion
		);

		assert_noop!(
			Broker::force_transfer(RuntimeOrigin::signed(1001), region_id, NEW_OWNER),
			BadOrigin
		);

		assert_ok!(Broker::force_transfer(RuntimeOrigin::root(), region_id, NEW_OWNER));

		System::assert_last_event(
			Event::Transferred {
				region_id,
				duration: region.end - region_id.begin,
				old_owner: Some(OLD_OWNER),
				owner: Some(NEW_OWNER),
			}
			.into(),
		);

		assert_noop!(
			Broker::assign(RuntimeOrigin::signed(OLD_OWNER), region_id, 10, Finality::Final),
			Error::<Test>::NotOwner
		);

		assert_ok!(Broker::assign(
			RuntimeOrigin::signed(NEW_OWNER),
			region_id,
			10,
			Finality::Final
		));
	});
}

#[test]
fn force_transfer_can_transfer_burned_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 4));
		advance_to(2);

		const OLD_OWNER: u64 = 1;
		const NEW_OWNER: u64 = 222;

		let region_id = Broker::do_purchase(OLD_OWNER, u64::max_value()).unwrap();

		assert_ok!(<Broker as Mutate<u64>>::burn(&region_id.into(), None));

		let region = Regions::<Test>::get(region_id).unwrap();
		assert_eq!(region.owner, None);

		assert_ok!(Broker::force_transfer(RuntimeOrigin::root(), region_id, NEW_OWNER));

		System::assert_last_event(
			Event::Transferred {
				region_id,
				duration: region.end - region_id.begin,
				old_owner: None,
				owner: Some(NEW_OWNER),
			}
			.into(),
		);

		assert_ok!(Broker::assign(
			RuntimeOrigin::signed(NEW_OWNER),
			region_id,
			10,
			Finality::Final
		));
	});
}

#[test]
fn force_transfer_can_transfer_provisionally_assigned_region() {
	TestExt::new().endow(1, 1000).execute_with(|| {
		assert_ok!(Broker::do_start_sales(100, 4));
		advance_to(2);

		const OLD_OWNER: u64 = 1;
		const NEW_OWNER: u64 = 222;

		let region_id = Broker::do_purchase(OLD_OWNER, u64::max_value()).unwrap();

		assert_ok!(Broker::assign(RuntimeOrigin::signed(OLD_OWNER), region_id, 1001, Provisional));

		assert_ok!(Broker::force_transfer(RuntimeOrigin::root(), region_id, NEW_OWNER));

		let region = Regions::<Test>::get(region_id).unwrap();
		System::assert_last_event(
			Event::Transferred {
				region_id,
				duration: region.end - region_id.begin,
				old_owner: Some(OLD_OWNER),
				owner: Some(NEW_OWNER),
			}
			.into(),
		);
	});
}
