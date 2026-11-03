// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

use super::{mock::*, transaction_extension::DynamicMaxBlockWeight, *};
use assert_matches::assert_matches;
use codec::Compact;
use cumulus_primitives_core::{
	BlockBundleInfo, ClaimQueueOffset, CoreInfo, CoreSelector, CumulusDigestItem,
};
use frame_support::{
	assert_ok,
	dispatch::{DispatchClass, DispatchInfo, PostDispatchInfo},
	pallet_prelude::{InvalidTransaction, TransactionSource},
	traits::PreInherents,
	weights::constants::WEIGHT_REF_TIME_PER_SECOND,
};
use frame_system::{CheckWeight, RawOrigin as SystemOrigin};
use polkadot_primitives::MAX_POV_SIZE;
use sp_core::ConstU32;
use sp_runtime::{
	traits::{DispatchTransaction, Header, TransactionExtension},
	Digest,
};

type TxExtension =
	DynamicMaxBlockWeight<Runtime, CheckWeight<Runtime>, ConstU32<TARGET_BLOCK_RATE>>;
type TxExtensionOnlyOperational =
	DynamicMaxBlockWeight<Runtime, CheckWeight<Runtime>, ConstU32<TARGET_BLOCK_RATE>, 10, false>;
type MaximumBlockWeight = MaxParachainBlockWeight<Runtime, ConstU32<TARGET_BLOCK_RATE>>;

#[test]
fn test_single_core_single_block() {
	TestExtBuilder::new().number_of_cores(1).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<1>>::get();

		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn test_single_core_multiple_blocks() {
	TestExtBuilder::new().number_of_cores(1).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::get();

		// With 1 core and 4 target blocks, should get 0.5s ref time and 1/4 PoV size per block
		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
		assert_eq!(weight.proof_size(), (1 * MAX_POV_SIZE as u64) / 4);
	});
}

#[test]
fn test_multiple_cores_single_block() {
	TestExtBuilder::new().number_of_cores(3).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<1>>::get();

		// With 3 cores and 1 target blocks, should get 2s ref time and 1 PoV size
		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn test_multiple_cores_multiple_blocks() {
	TestExtBuilder::new().number_of_cores(2).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::get();

		// With 2 cores and 4 target blocks, should get 1s ref time and 2x PoV size / 4 per
		// block
		assert_eq!(weight.ref_time(), 2 * 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
		assert_eq!(weight.proof_size(), (2 * MAX_POV_SIZE as u64) / 4);
	});
}

#[test]
fn test_no_core_info() {
	TestExtBuilder::new().build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::get();

		// Without core info, it takes the `PreviousCoreCount` into account.
		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64 / 4);
	});
}

#[test]
fn test_zero_cores() {
	TestExtBuilder::new().number_of_cores(0).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::get();

		// With 0 cores, should return conservative default
		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn test_uneven_number_of_blocks_on_even_number_of_cores() {
	TestExtBuilder::new().number_of_cores(2).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<3>>::get();

		// Each block should get half of a core.
		assert_eq!(weight.ref_time(), WEIGHT_REF_TIME_PER_SECOND);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64 / 2);
	});
}

#[test]
fn test_zero_target_blocks() {
	TestExtBuilder::new().number_of_cores(2).build().execute_with(|| {
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<0>>::get();
		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn test_target_block_weight_calculation() {
	TestExtBuilder::new().number_of_cores(4).build().execute_with(|| {
		// Test target_block_weight function directly
		// Both calls return the same since ConstU32<4> is fixed at compile time
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::target_block_weight();

		assert_eq!(weight.ref_time(), 3 * 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn test_max_ref_time_per_core_cap() {
	TestExtBuilder::new().number_of_cores(8).build().execute_with(|| {
		// With 8 cores and 4 target blocks, ref time per block should be capped at 2s per core
		let weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::get();

		// 8 cores * 2s = 16s total, divided by 4 blocks = 4s, but capped at 6s for all blocks in
		// total
		assert_eq!(weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND * 3 / 4);
		assert_eq!(weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn test_target_block_weight_with_digest_edge_cases() {
	TestExtBuilder::new().build().execute_with(|| {
		// Test with empty digest
		let empty_digest = Digest::default();
		let weight =
			MaxParachainBlockWeight::<Runtime, ConstU32<4>>::target_block_weight_with_digest(
				&empty_digest,
			);
		assert_eq!(weight, FULL_CORE_WEIGHT / 4);

		// Test with digest containing core info
		let core_info = CoreInfo {
			selector: CoreSelector(0),
			claim_queue_offset: ClaimQueueOffset(0),
			number_of_cores: Compact(2u16),
		};

		let digest = Digest { logs: vec![CumulusDigestItem::CoreInfo(core_info).to_digest_item()] };

		// With 2 cores and 4 target blocks: (2 cores * 2s) / 4 blocks = 1s
		let weight =
			MaxParachainBlockWeight::<Runtime, ConstU32<4>>::target_block_weight_with_digest(
				&digest,
			);
		assert_eq!(weight.ref_time(), 2 * 2 * WEIGHT_REF_TIME_PER_SECOND / 4);
		assert_eq!(weight.proof_size(), (2 * MAX_POV_SIZE as u64) / 4);
	});
}

#[test]
fn test_is_first_block_in_core_functions() {
	TestExtBuilder::new().number_of_cores(1).build().execute_with(|| {
		let empty_digest = Digest::default();
		assert!(super::is_first_block_in_core_with_digest(&empty_digest).is_none());

		// Test with bundle info index = 0 - should return true
		let bundle_info_first = BlockBundleInfo { index: 0, is_last: false };
		let digest_item_first =
			CumulusDigestItem::BlockBundleInfo(bundle_info_first).to_digest_item();
		let mut digest_first = Digest::default();
		digest_first.push(digest_item_first);
		assert!(super::is_first_block_in_core_with_digest(&digest_first).unwrap());

		// Test with bundle info index > 0 - should return false
		let bundle_info_not_first = BlockBundleInfo { index: 5, is_last: true };
		let digest_item_not_first =
			CumulusDigestItem::BlockBundleInfo(bundle_info_not_first).to_digest_item();
		let mut digest_not_first = Digest::default();
		digest_not_first.push(digest_item_not_first);
		assert!(!super::is_first_block_in_core_with_digest(&digest_not_first).unwrap());
	});
}

#[test]
fn tx_extension_sets_fraction_of_core_mode() {
	use frame_support::dispatch::{DispatchClass, DispatchInfo};

	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			// Create a small transaction
			let small_weight = Weight::from_parts(100_000, 1024);
			let info = DispatchInfo {
				call_weight: small_weight,
				class: DispatchClass::Normal,
				pays_fee: frame_support::dispatch::Pays::Yes,
				..Default::default()
			};

			assert_ok!(TxExtension::validate_and_prepare(
				TxExtension::new(Default::default()),
				SystemOrigin::Signed(0).into(),
				&CALL,
				&info,
				100,
				0,
			));

			assert_eq!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::fraction_of_core(Some(0)))
			);
		});
}

#[test]
fn tx_extension_large_tx_enables_full_core_usage() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			// Create a transaction larger than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let large_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			let info = DispatchInfo {
				call_weight: large_weight,
				class: DispatchClass::Normal,
				..Default::default()
			};

			System::set_extrinsic_index(1);

			assert_ok!(TxExtension::validate_and_prepare(
				TxExtension::new(Default::default()),
				SystemOrigin::Signed(0).into(),
				&CALL,
				&info,
				100,
				0,
			));

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::PotentialFullCore { first_transaction_index: Some(1), .. })
			);

			let mut post_info =
				PostDispatchInfo { actual_weight: None, pays_fee: Default::default() };

			assert_ok!(TxExtension::post_dispatch((), &info, &mut post_info, 0, &Ok(())));

			assert_eq!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::full_core())
			);
			assert!(has_use_full_core_digest());
			assert_eq!(MaximumBlockWeight::get().ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		});
}

#[test]
fn tx_extension_only_allows_large_operational_tx_to_enable_full_core_usage() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			// Create a transaction larger than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let large_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			let mut info = DispatchInfo {
				call_weight: large_weight,
				class: DispatchClass::Normal,
				..Default::default()
			};

			// As `Normal` transaction this should be rejected.
			assert_eq!(
				TxExtensionOnlyOperational::validate_and_prepare(
					TxExtensionOnlyOperational::new(Default::default()),
					SystemOrigin::Signed(0).into(),
					&CALL,
					&info,
					100,
					0,
				)
				.unwrap_err(),
				InvalidTransaction::ExhaustsResources.into()
			);

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FractionOfCore { first_transaction_index: None, .. })
			);

			info.class = DispatchClass::Operational;

			// As `Operational` transaction this is accepted.
			assert_ok!(TxExtensionOnlyOperational::validate_and_prepare(
				TxExtensionOnlyOperational::new(Default::default()),
				SystemOrigin::Signed(0).into(),
				&CALL,
				&info,
				100,
				0,
			));

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::PotentialFullCore { first_transaction_index: Some(0), .. })
			);

			let mut post_info =
				PostDispatchInfo { actual_weight: None, pays_fee: Default::default() };

			assert_ok!(TxExtension::post_dispatch((), &info, &mut post_info, 0, &Ok(())));

			assert_eq!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::full_core())
			);
			assert!(has_use_full_core_digest());
			assert_eq!(MaximumBlockWeight::get().ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		});
}

#[test]
fn tx_extension_large_tx_with_refund_goes_back_to_fractional() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			System::set_extrinsic_index(1);

			// Create a transaction larger than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let large_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			let info = DispatchInfo {
				call_weight: large_weight,
				class: DispatchClass::Normal,
				..Default::default()
			};

			assert_ok!(TxExtension::validate_and_prepare(
				TxExtension::new(Default::default()),
				SystemOrigin::Signed(0).into(),
				&CALL,
				&info,
				100,
				0,
			));

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::PotentialFullCore { first_transaction_index: Some(1), .. })
			);

			let mut post_info = PostDispatchInfo {
				actual_weight: Some(Weight::from_parts(5000, 5000)),
				pays_fee: Default::default(),
			};

			assert_ok!(TxExtension::post_dispatch((), &info, &mut post_info, 0, &Ok(())));

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FractionOfCore { .. })
			);
			assert!(!has_use_full_core_digest());
			assert_eq!(MaximumBlockWeight::get(), target_weight);
		});
}

#[test]
fn tx_extension_large_tx_is_rejected_on_non_first_block() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(false)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			// Create a transaction larger than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let large_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			let info = DispatchInfo {
				call_weight: large_weight,
				class: DispatchClass::Normal,
				..Default::default()
			};

			assert_eq!(
				TxExtension::validate_and_prepare(
					TxExtension::new(Default::default()),
					SystemOrigin::Signed(0).into(),
					&CALL,
					&info,
					100,
					0,
				)
				.unwrap_err(),
				InvalidTransaction::ExhaustsResources.into()
			);

			// Should stay in FractionOfCore mode (not PotentialFullCore) since not first block
			assert_eq!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::fraction_of_core(None))
			);
			assert!(!has_use_full_core_digest());
			assert_eq!(MaximumBlockWeight::get(), target_weight);
		});
}

#[test]
fn tx_extension_post_dispatch_to_full_core_because_of_manual_weight() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(false)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			let target_weight =
				MaxParachainBlockWeight::<Runtime, ConstU32<4>>::target_block_weight();

			// Transaction announces small weight
			let small_weight = Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND / 10, 1024);
			let info = DispatchInfo { call_weight: small_weight, ..Default::default() };

			assert_ok!(TxExtension::validate_and_prepare(
				TxExtension::new(Default::default()),
				SystemOrigin::Signed(0).into(),
				&CALL,
				&info,
				100,
				0,
			));

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FractionOfCore { first_transaction_index: Some(0), .. })
			);

			// But actually uses much more weight (bug in weight annotation)
			let large_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));
			register_weight(large_weight, DispatchClass::Normal);

			let mut post_info =
				PostDispatchInfo { actual_weight: None, pays_fee: Default::default() };
			assert_ok!(TxExtension::post_dispatch((), &info, &mut post_info, 0, &Ok(())));

			// Should transition to FullCore due to exceeding limit
			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FullCore { .. })
			);

			assert!(has_use_full_core_digest());
		});
}

#[test]
fn tx_extension_large_tx_after_limit_is_rejected() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			initialize_block_finished();

			// Set some index above the limit.
			System::set_extrinsic_index(20);

			// Create a transaction larger than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let large_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			let info = DispatchInfo { call_weight: large_weight, ..Default::default() };

			assert_eq!(
				TxExtension::validate_and_prepare(
					TxExtension::new(Default::default()),
					SystemOrigin::Signed(0).into(),
					&CALL,
					&info,
					100,
					0,
				)
				.unwrap_err(),
				InvalidTransaction::ExhaustsResources.into()
			);

			assert_eq!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::fraction_of_core(None))
			);
			assert!(!has_use_full_core_digest());
		});
}

#[test]
fn tx_extension_large_weight_before_first_tx() {
	for first_block_in_core in [true, false] {
		TestExtBuilder::new()
			.number_of_cores(2)
			.first_block_in_core(first_block_in_core)
			.build()
			.execute_with(|| {
				initialize_block_finished();

				let target_weight = MaximumBlockWeight::target_block_weight();
				let large_weight = target_weight
					.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

				register_weight(large_weight, DispatchClass::Normal);

				let small_weight = Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND / 10, 1024);
				let info = DispatchInfo { call_weight: small_weight, ..Default::default() };

				let res = TxExtension::validate_and_prepare(
					TxExtension::new(Default::default()),
					SystemOrigin::Signed(0).into(),
					&CALL,
					&info,
					100,
					0,
				);

				if first_block_in_core {
					assert!(res.is_ok())
				} else {
					assert_eq!(res.unwrap_err(), InvalidTransaction::ExhaustsResources.into());
				}

				assert_matches!(
					crate::BlockWeightMode::<Runtime>::get(),
					Some(BlockWeightMode::FullCore { .. })
				);

				assert!(has_use_full_core_digest());
				assert_eq!(MaximumBlockWeight::get().ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);

				if !first_block_in_core {
					// Should have registered FULL_CORE_WEIGHT to prevent more transactions
					let final_remaining = frame_system::Pallet::<Runtime>::remaining_block_weight();
					assert!(final_remaining.consumed().all_gte(FULL_CORE_WEIGHT));
				}
			});
	}
}

#[test]
fn pre_inherents_hook_first_block_over_limit() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			// Simulate on_initialize consuming more than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let excessive_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			register_weight(excessive_weight, DispatchClass::Mandatory);

			// Call pre_inherents hook
			DynamicMaxBlockWeightHooks::<Runtime, ConstU32<4>>::pre_inherents();

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FullCore { .. })
			);

			// Should have UseFullCore digest
			assert!(has_use_full_core_digest());
		});
}

#[test]
fn pre_inherents_hook_non_first_block_over_limit() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(false)
		.build()
		.execute_with(|| {
			// Simulate on_initialize consuming more than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let excessive_weight = target_weight
				.saturating_add(Weight::from_parts(WEIGHT_REF_TIME_PER_SECOND, 1024 * 1024));

			register_weight(excessive_weight, DispatchClass::Mandatory);

			// Call pre_inherents hook
			DynamicMaxBlockWeightHooks::<Runtime, ConstU32<4>>::pre_inherents();

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FullCore { .. })
			);

			assert!(has_use_full_core_digest());

			// Should have registered FULL_CORE_WEIGHT to prevent more transactions
			let final_remaining = frame_system::Pallet::<Runtime>::remaining_block_weight();
			assert!(final_remaining.consumed().all_gte(FULL_CORE_WEIGHT));
		});
}

#[test]
fn pre_inherents_hook_under_limit_no_change() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			// Simulate on_initialize consuming less than target weight
			let target_weight = MaximumBlockWeight::target_block_weight();
			let small_weight =
				Weight::from_parts(target_weight.ref_time() / 2, target_weight.proof_size() / 2);

			register_weight(small_weight, DispatchClass::Mandatory);

			// Call pre_inherents hook
			DynamicMaxBlockWeightHooks::<Runtime, ConstU32<4>>::pre_inherents();

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::FractionOfCore { first_transaction_index: None, .. })
			);

			// Should NOT have UseFullCore digest
			assert!(!has_use_full_core_digest());
		});
}

#[test]
fn max_weight_without_bundle_info() {
	TestExtBuilder::new().number_of_cores(2).build().execute_with(|| {
		// Without bundle info, cannot determine if first block
		// Should still work but max weight determination will be conservative

		frame_system::Pallet::<Runtime>::note_finished_initialize();

		let max_weight = MaximumBlockWeight::get();

		// With 2 cores and 12 target blocks
		let expected_weight = Weight::from_parts(
			2 * 2 * WEIGHT_REF_TIME_PER_SECOND / TARGET_BLOCK_RATE as u64,
			2 * MAX_POV_SIZE as u64 / TARGET_BLOCK_RATE as u64,
		);

		assert_eq!(max_weight, expected_weight);
	});
}

#[test]
fn ref_time_and_pov_size_cap() {
	TestExtBuilder::new().number_of_cores(10).build().execute_with(|| {
		frame_system::Pallet::<Runtime>::note_finished_initialize();

		let max_weight = MaxParachainBlockWeight::<Runtime, ConstU32<1>>::get();

		// At most one core will always only be able to use the resources of one core.
		assert_eq!(max_weight.ref_time(), 2 * WEIGHT_REF_TIME_PER_SECOND);
		assert_eq!(max_weight.proof_size(), MAX_POV_SIZE as u64);

		let max_weight = MaxParachainBlockWeight::<Runtime, ConstU32<4>>::get();

		// Each blocks get its own core (can use the max pov size), but ref time of all blocks
		// together is in max `6s`
		assert_eq!(max_weight.ref_time(), 6 * WEIGHT_REF_TIME_PER_SECOND / 4);
		assert_eq!(max_weight.proof_size(), MAX_POV_SIZE as u64);
	});
}

#[test]
fn executive_validate_block_handles_normal_transactions() {
	for signed in [true, false] {
		TestExtBuilder::new().previous_core_count(3).build().execute_with(|| {
			let call = RuntimeCall::TestPallet(test_pallet::Call::heavy_call_normal {});

			let xt = if signed {
				Extrinsic::new_signed(call, 1u64.into(), 1u64.into(), Default::default())
			} else {
				Extrinsic::new_bare(call)
			};

			assert_ok!(Executive::validate_transaction(
				TransactionSource::External,
				xt.clone(),
				Default::default()
			));
		});

		TestExtBuilder::new().previous_core_count(3).build().execute_with(|| {
			let call =
				RuntimeCallOnlyOperational::TestPallet(test_pallet::Call::heavy_call_normal {});

			let xt = if signed {
				ExtrinsicOnlyOperational::new_signed(
					call,
					1u64.into(),
					1u64.into(),
					Default::default(),
				)
			} else {
				ExtrinsicOnlyOperational::new_bare(call)
			};

			assert_eq!(
				ExecutiveOnlyOperational::validate_transaction(
					TransactionSource::External,
					xt,
					Default::default()
				)
				.unwrap_err(),
				InvalidTransaction::ExhaustsResources.into()
			);
		});
	}
}

#[test]
fn executive_validate_block_handles_operational_transactions() {
	for signed in [true, false] {
		TestExtBuilder::new().previous_core_count(3).build().execute_with(|| {
			let call = RuntimeCall::TestPallet(test_pallet::Call::heavy_call_operational {});

			let xt = if signed {
				Extrinsic::new_signed(call, 1u64.into(), 1u64.into(), Default::default())
			} else {
				Extrinsic::new_bare(call)
			};

			assert_ok!(Executive::validate_transaction(
				TransactionSource::External,
				xt.clone(),
				Default::default()
			));
		});

		TestExtBuilder::new().previous_core_count(3).build().execute_with(|| {
			let call = RuntimeCallOnlyOperational::TestPallet(
				test_pallet::Call::heavy_call_operational {},
			);

			let xt = if signed {
				ExtrinsicOnlyOperational::new_signed(
					call,
					1u64.into(),
					1u64.into(),
					Default::default(),
				)
			} else {
				ExtrinsicOnlyOperational::new_bare(call)
			};

			assert!(ExecutiveOnlyOperational::validate_transaction(
				TransactionSource::External,
				xt,
				Default::default()
			)
			.is_ok());
		});
	}
}

#[test]
fn executive_with_operational_only_applies_big_inherent() {
	TestExtBuilder::new()
		.number_of_cores(1)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			Executive::initialize_block(&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				System::digest(),
			));

			let call =
				RuntimeCallOnlyOperational::TestPallet(test_pallet::Call::heavy_call_mandatory {});

			let xt = ExtrinsicOnlyOperational::new_bare(call);

			ExecutiveOnlyOperational::apply_extrinsic(xt).unwrap().unwrap();
		});
}

#[test]
fn block_weight_mode_from_previous_block_is_ignored_in_validate_block() {
	TestExtBuilder::new()
		.number_of_cores(4)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			let call = RuntimeCallOnlyOperational::TestPallet(
				test_pallet::Call::heavy_call_operational {},
			);

			let xt = ExtrinsicOnlyOperational::new_signed(
				call,
				1u64.into(),
				1u64.into(),
				Default::default(),
			);

			ExecutiveOnlyOperational::initialize_block(&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				System::digest(),
			));

			assert_ok!(ExecutiveOnlyOperational::apply_extrinsic(xt));

			fake_set_validation_data();

			ExecutiveOnlyOperational::finalize_block();

			assert_eq!(
				crate::BlockWeightMode::<RuntimeOnlyOperational>::get().unwrap(),
				BlockWeightMode::full_core()
			);

			let call =
				RuntimeCallOnlyOperational::TestPallet(test_pallet::Call::heavy_call_normal {});

			let xt = ExtrinsicOnlyOperational::new_signed(
				call,
				1u64.into(),
				1u64.into(),
				Default::default(),
			);

			assert_eq!(
				ExecutiveOnlyOperational::validate_transaction(
					TransactionSource::External,
					xt,
					Default::default()
				)
				.unwrap_err(),
				InvalidTransaction::ExhaustsResources.into()
			);
		});
}

#[test]
fn ongoing_mbm_requests_full_core() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			MbmOngoing::set(true);
			ExecutiveOnlyOperational::initialize_block(&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				System::digest(),
			));

			assert_eq!(
				FULL_CORE_WEIGHT,
				<RuntimeOnlyOperational as frame_system::Config>::BlockWeights::get().max_block
			);

			fake_set_validation_data();

			ExecutiveOnlyOperational::finalize_block();

			assert!(has_use_full_core_digest());
			MbmOngoing::set(false);
		});
}

#[test]
fn ignores_previous_block_weight_in_on_initialize() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			crate::BlockWeightMode::<RuntimeOnlyOperational>::put(
				BlockWeightMode::fraction_of_core(None),
			);

			// Start a new block
			System::set_block_number(1);

			assert_eq!(MaximumBlockWeight::get(), FULL_CORE_WEIGHT);
		});
}

#[test]
fn full_core_weight_in_inherent_context() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			Executive::initialize_block(&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				System::digest(),
			));

			assert!(!frame_system::Pallet::<RuntimeOnlyOperational>::inherents_applied());

			assert_eq!(MaximumBlockWeight::get(), FULL_CORE_WEIGHT);
		});
}

#[test]
fn executive_validate_transaction_respects_dispatch_class_max_block_size() {
	// Create some weight which is slightly above the allowed dispatch class max size.
	let call_weight = TestExtBuilder::new().previous_core_count(4).build().execute_with(|| {
		MaximumBlockWeight::target_block_weight() * NORMAL_DISPATCH_RATIO + Weight::from_parts(1, 1)
	});

	for signed in [true, false] {
		TestExtBuilder::new().previous_core_count(4).build().execute_with(|| {
			assert!(<RuntimeOnlyOperational as frame_system::Config>::BlockWeights::get()
				.get(DispatchClass::Normal)
				.max_total
				.unwrap()
				.all_lt(call_weight));
			assert!(MaximumBlockWeight::target_block_weight().all_gt(call_weight));

			let call =
				RuntimeCall::TestPallet(test_pallet::Call::use_weight { weight: call_weight });

			let xt = if signed {
				Extrinsic::new_signed(call, 1u64.into(), 1u64.into(), Default::default())
			} else {
				Extrinsic::new_bare(call)
			};

			assert_ok!(Executive::validate_transaction(
				TransactionSource::External,
				xt.clone(),
				Default::default()
			));
		});

		TestExtBuilder::new().previous_core_count(4).build().execute_with(|| {
			let call = RuntimeCallOnlyOperational::TestPallet(test_pallet::Call::use_weight {
				weight: call_weight,
			});

			let xt = if signed {
				ExtrinsicOnlyOperational::new_signed(
					call,
					1u64.into(),
					1u64.into(),
					Default::default(),
				)
			} else {
				ExtrinsicOnlyOperational::new_bare(call)
			};

			assert_eq!(
				ExecutiveOnlyOperational::validate_transaction(
					TransactionSource::External,
					xt,
					Default::default()
				)
				.unwrap_err(),
				InvalidTransaction::ExhaustsResources.into()
			);
		});
	}
}

#[test]
fn on_idle_uses_correct_weight() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			Executive::initialize_block(&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				System::digest(),
			));

			fake_set_validation_data();

			OnIdleMaxLeftWeight::set(Some(MaximumBlockWeight::target_block_weight()));

			Executive::finalize_block();
		});
}

#[test]
fn on_poll_uses_correct_weight() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			Executive::initialize_block(&Header::new(
				1,
				Default::default(),
				Default::default(),
				Default::default(),
				System::digest(),
			));

			fake_set_validation_data();

			OnPollMaxLeftWeight::set(Some(MaximumBlockWeight::target_block_weight()));

			Executive::finalize_block();
		});
}

// This test ensures when a transaction enables `PotentialFullCore` in `pre-dispatch`, but in post
// dispatch the transaction has a lower weight we don't go back to `FractionalCore` if the total
// block weight is above the target block weight.
#[test]
fn post_dispatch_is_taking_block_weight_into_account() {
	TestExtBuilder::new()
		.number_of_cores(2)
		.first_block_in_core(true)
		.build()
		.execute_with(|| {
			initialize_block_finished();
			System::set_extrinsic_index(1);

			let target_weight = MaximumBlockWeight::target_block_weight();

			let sixty_percent = Weight::from_parts(
				target_weight.ref_time() * 60 / 100,
				target_weight.proof_size() * 60 / 100,
			);

			// Simulate on_initialize using 60% of target weight
			register_weight(sixty_percent, DispatchClass::Mandatory);

			let info = DispatchInfo {
				call_weight: target_weight,
				class: DispatchClass::Normal,
				..Default::default()
			};

			assert_ok!(TxExtension::validate_and_prepare(
				TxExtension::new(Default::default()),
				SystemOrigin::Signed(0).into(),
				&CALL,
				&info,
				100,
				0,
			));

			assert_matches!(
				crate::BlockWeightMode::<Runtime>::get(),
				Some(BlockWeightMode::PotentialFullCore { .. })
			);

			// Post-dispatch with actual_weight = 60% of target
			let mut post_info = PostDispatchInfo {
				actual_weight: Some(sixty_percent),
				pays_fee: Default::default(),
			};

			assert_ok!(TxExtension::post_dispatch((), &info, &mut post_info, 0, &Ok(())));

			assert!(has_use_full_core_digest());
		});
}
