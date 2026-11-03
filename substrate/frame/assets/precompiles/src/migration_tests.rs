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

//! Tests for `MigrateForeignAssetPrecompileMappings`.
//!
//! This module defines its own mock runtime that uses `xcm::v5::Location` as the asset ID type,
//! which is the real-world use case for foreign assets.

use crate::{foreign_assets::pallet, migration::MigrateForeignAssetPrecompileMappings};
use frame_support::{
	derive_impl,
	migrations::SteppedMigration,
	parameter_types,
	traits::AsEnsureOriginWithArg,
	weights::{Weight, WeightMeter},
};
use pallet_assets::AssetDetails;
use sp_runtime::BuildStorage;
use xcm::v5::{Junction, Location};

/// A benchmark helper that creates Location instances from u32 IDs.
#[cfg(feature = "runtime-benchmarks")]
pub struct LocationBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_assets::BenchmarkHelper<Location, ()> for LocationBenchmarkHelper {
	fn create_asset_id_parameter(id: u32) -> Location {
		Location::new(1, [Junction::Parachain(id)])
	}
	fn create_reserve_id_parameter(_id: u32) {}
}

// Define a mock runtime that uses Location as the asset ID
#[frame_support::runtime]
mod runtime {
	#[runtime::runtime]
	#[runtime::derive(
		RuntimeCall,
		RuntimeEvent,
		RuntimeError,
		RuntimeOrigin,
		RuntimeTask,
		RuntimeHoldReason,
		RuntimeFreezeReason
	)]
	pub struct Test;

	#[runtime::pallet_index(0)]
	pub type System = frame_system;
	#[runtime::pallet_index(10)]
	pub type Balances = pallet_balances;
	#[runtime::pallet_index(20)]
	pub type ForeignAssets = pallet_assets<Instance1>;
	#[runtime::pallet_index(22)]
	pub type AssetsPrecompiles = crate::foreign_assets;
}

pub type ForeignAssetsInstance = pallet_assets::Instance1;

type Block = frame_system::mocking::MockBlock<Test>;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<u128>;
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig as pallet_balances::DefaultConfig)]
impl pallet_balances::Config for Test {
	type Balance = u128;
	type AccountStore = System;
}

parameter_types! {
	pub const AssetDeposit: u128 = 1;
	pub const AssetAccountDeposit: u128 = 1;
	pub const ApprovalDeposit: u128 = 1;
	pub const MetadataDepositBase: u128 = 1;
	pub const MetadataDepositPerByte: u128 = 1;
}

impl pallet_assets::Config<ForeignAssetsInstance> for Test {
	type RuntimeEvent = RuntimeEvent;
	type Balance = u128;
	type AssetId = Location;
	type AssetIdParameter = Location;
	type CreateOrigin = AsEnsureOriginWithArg<frame_system::EnsureSigned<u64>>;
	type ForceOrigin = frame_system::EnsureRoot<u64>;
	type Currency = Balances;
	type AssetDeposit = AssetDeposit;
	type AssetAccountDeposit = AssetAccountDeposit;
	type MetadataDepositBase = MetadataDepositBase;
	type MetadataDepositPerByte = MetadataDepositPerByte;
	type ApprovalDeposit = ApprovalDeposit;
	type StringLimit = frame_support::traits::ConstU32<50>;
	type Freezer = ();
	type Extra = ();
	type WeightInfo = ();
	type RemoveItemsLimit = frame_support::traits::ConstU32<1000>;
	type CallbackHandle = ();
	type AssetIdAllocator = ();
	type ReserveData = ();
	type Holder = ();
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = LocationBenchmarkHelper;
}

impl pallet::Config for Test {
	type ForeignAssetId = Location;
	#[cfg(feature = "runtime-benchmarks")]
	type AssetsInstance = ForeignAssetsInstance;
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	let mut ext: sp_io::TestExternalities = t.into();
	ext.execute_with(|| {
		frame_system::Pallet::<Test>::set_block_number(1);
	});
	ext
}

/// Helper to run the stepped migration to completion.
fn run_migration_to_completion() -> u32 {
	let mut cursor = None;
	let mut steps = 0u32;
	loop {
		let mut meter = WeightMeter::with_limit(Weight::MAX);
		match MigrateForeignAssetPrecompileMappings::<Test, ForeignAssetsInstance, ()>::step(
			cursor, &mut meter,
		) {
			Ok(None) => break, // Migration complete
			Ok(Some(new_cursor)) => {
				cursor = Some(new_cursor);
				steps = steps.saturating_add(1);
			},
			Err(e) => panic!("Migration failed: {:?}", e),
		}
	}
	steps
}

/// Helper to create a minimal AssetDetails for testing.
fn create_asset_details(
	owner: <Test as frame_system::Config>::AccountId,
) -> AssetDetails<
	<Test as pallet_assets::Config<ForeignAssetsInstance>>::Balance,
	<Test as frame_system::Config>::AccountId,
	<Test as pallet_balances::Config>::Balance,
> {
	AssetDetails {
		owner,
		issuer: owner,
		admin: owner,
		freezer: owner,
		supply: 0,
		deposit: 0,
		min_balance: 1,
		is_sufficient: false,
		accounts: 0,
		sufficients: 0,
		approvals: 0,
		status: pallet_assets::AssetStatus::Live,
	}
}

/// Test that the migration correctly populates precompile mappings for foreign assets.
///
/// This test:
/// 1. Creates foreign assets directly in storage WITHOUT precompile mappings (simulating the
///    pre-migration state)
/// 2. Verifies that precompile mappings do NOT exist before migration
/// 3. Runs the migration
/// 4. Verifies that precompile mappings exist after migration with sequential indices
#[test]
fn migration_populates_precompile_mappings() {
	new_test_ext().execute_with(|| {
		let owner = 1u64;

		// Create foreign asset locations (simulating assets from sibling parachains)
		let sibling_para_1000_asset = Location::new(1, [Junction::Parachain(1000)]);
		let sibling_para_2000_asset = Location::new(1, [Junction::Parachain(2000)]);
		let sibling_para_3000_asset = Location::new(1, [Junction::Parachain(3000)]);

		let test_assets = vec![
			sibling_para_1000_asset.clone(),
			sibling_para_2000_asset.clone(),
			sibling_para_3000_asset.clone(),
		];

		// Insert foreign assets directly into pallet_assets storage without creating precompile
		// mappings
		for asset_location in &test_assets {
			pallet_assets::Asset::<Test, ForeignAssetsInstance>::insert(
				asset_location.clone(),
				create_asset_details(owner),
			);
		}

		// Verify assets are in storage
		for asset_location in &test_assets {
			assert!(
				pallet_assets::Asset::<Test, ForeignAssetsInstance>::contains_key(asset_location),
				"Asset should exist in pallet_assets storage"
			);
		}

		// Verify precompile mappings do NOT exist before migration
		for asset_location in &test_assets {
			assert!(
				pallet::Pallet::<Test>::asset_index_of(asset_location).is_none(),
				"Precompile mapping should NOT exist before migration"
			);
		}

		run_migration_to_completion();

		// Verify precompile mappings now exist after migration
		// All assets should have sequential indices (0, 1, 2, ...)
		let mut indices_found = Vec::new();
		for asset_location in &test_assets {
			// Check reverse mapping exists: location -> index
			let stored_index = pallet::Pallet::<Test>::asset_index_of(asset_location);
			assert!(
				stored_index.is_some(),
				"Reverse precompile mapping should exist after migration for {:?}",
				asset_location
			);

			let index = stored_index.unwrap();
			indices_found.push(index);

			// Check forward mapping is consistent: index -> location
			let stored_location = pallet::Pallet::<Test>::asset_id_of(index);
			assert_eq!(
				stored_location,
				Some(asset_location.clone()),
				"Forward precompile mapping should be consistent for {:?}",
				asset_location
			);
		}

		// Verify indices are sequential starting from 0
		indices_found.sort();
		assert_eq!(indices_found.len(), 3);
		// The indices should be 0, 1, 2 (order depends on storage iteration)
		assert!(indices_found.iter().all(|&i| i < 3), "All indices should be less than 3");
	});
}

/// Test that the migration handles the case with no foreign assets gracefully.
#[test]
fn migration_handles_empty_foreign_assets() {
	new_test_ext().execute_with(|| {
		let steps = run_migration_to_completion();
		// With no assets, the migration should complete without processing any assets
		assert_eq!(steps, 0, "Empty migration should have no steps");
	});
}

/// Test that assets with existing mappings are correctly skipped.
#[test]
fn migration_skips_already_mapped_assets() {
	new_test_ext().execute_with(|| {
		let owner = 1u64;

		let asset_with_mapping = Location::new(1, [Junction::Parachain(1111)]);
		let asset_without_mapping = Location::new(1, [Junction::Parachain(2222)]);

		pallet_assets::Asset::<Test, ForeignAssetsInstance>::insert(
			asset_with_mapping.clone(),
			create_asset_details(owner),
		);
		pallet_assets::Asset::<Test, ForeignAssetsInstance>::insert(
			asset_without_mapping.clone(),
			create_asset_details(owner),
		);

		// Pre-create mapping for one asset using insert_asset_mapping
		let pre_mapped_index =
			pallet::Pallet::<Test>::insert_asset_mapping(&asset_with_mapping).unwrap();

		run_migration_to_completion();

		// The pre-mapped asset should keep its original index
		assert_eq!(
			pallet::Pallet::<Test>::asset_index_of(&asset_with_mapping),
			Some(pre_mapped_index),
			"Pre-existing mapping should be preserved"
		);
		assert_eq!(
			pallet::Pallet::<Test>::asset_id_of(pre_mapped_index),
			Some(asset_with_mapping),
			"Pre-existing forward mapping should be preserved"
		);

		// The unmapped asset should now have a mapping
		assert!(
			pallet::Pallet::<Test>::asset_index_of(&asset_without_mapping).is_some(),
			"New mapping should be created for previously unmapped asset"
		);
	});
}

/// Test that the migration respects weight limits and can be resumed.
#[test]
fn migration_respects_weight_limits() {
	new_test_ext().execute_with(|| {
		let owner = 1u64;

		// Create multiple foreign assets
		let assets: Vec<Location> = (1000..1005u32)
			.map(|para_id| Location::new(1, [Junction::Parachain(para_id)]))
			.collect();

		for asset_location in &assets {
			pallet_assets::Asset::<Test, ForeignAssetsInstance>::insert(
				asset_location.clone(),
				create_asset_details(owner),
			);
		}

		// Run with limited weight - should process assets in steps
		let mut cursor = None;
		let mut steps = 0u32;

		// Use a weight that allows processing ~2 assets per step
		// The default WeightInfo for () returns ~423M ref_time and ~6360 proof_size per step
		let limited_weight = Weight::from_parts(900_000_000, 10_000);

		loop {
			let mut meter = WeightMeter::with_limit(limited_weight);

			match MigrateForeignAssetPrecompileMappings::<Test, ForeignAssetsInstance, ()>::step(
				cursor, &mut meter,
			) {
				Ok(None) => break,
				Ok(Some(new_cursor)) => {
					cursor = Some(new_cursor);
					steps = steps.saturating_add(1);
				},
				Err(e) => panic!("Migration failed: {:?}", e),
			}
		}

		// Verify all assets were migrated
		for asset_location in &assets {
			assert!(
				pallet::Pallet::<Test>::asset_index_of(asset_location).is_some(),
				"Asset {:?} should have a mapping after migration",
				asset_location
			);
		}
	});
}

/// Test that migration ID is consistent.
#[test]
fn migration_id_is_consistent() {
	let id1 = MigrateForeignAssetPrecompileMappings::<Test, ForeignAssetsInstance, ()>::id();
	let id2 = MigrateForeignAssetPrecompileMappings::<Test, ForeignAssetsInstance, ()>::id();

	// Compare individual fields since MigrationId doesn't implement PartialEq
	assert_eq!(id1.pallet_id, id2.pallet_id, "Pallet ID should be consistent");
	assert_eq!(id1.version_from, id2.version_from, "Version from should be consistent");
	assert_eq!(id1.version_to, id2.version_to, "Version to should be consistent");

	assert_eq!(
		&id1.pallet_id, b"foreign-asset-precompile-mapping",
		"Migration ID should have correct pallet_id"
	);
	assert_eq!(id1.version_from, 0, "Migration should be from version 0");
	assert_eq!(id1.version_to, 1, "Migration should be to version 1");
}

/// Test that sequential indices are correctly assigned during migration.
#[test]
fn migration_assigns_sequential_indices() {
	new_test_ext().execute_with(|| {
		let owner = 1u64;

		// Create 5 assets
		let assets: Vec<Location> = (1000..1005u32)
			.map(|para_id| Location::new(1, [Junction::Parachain(para_id)]))
			.collect();

		for asset_location in &assets {
			pallet_assets::Asset::<Test, ForeignAssetsInstance>::insert(
				asset_location.clone(),
				create_asset_details(owner),
			);
		}

		// NextAssetIndex should be 0 initially
		assert_eq!(pallet::Pallet::<Test>::next_asset_index(), 0);

		run_migration_to_completion();

		// NextAssetIndex should be 5 after migrating 5 assets
		assert_eq!(pallet::Pallet::<Test>::next_asset_index(), 5);

		// Collect all assigned indices
		let mut assigned_indices: Vec<u32> = assets
			.iter()
			.map(|loc| pallet::Pallet::<Test>::asset_index_of(loc).unwrap())
			.collect();
		assigned_indices.sort();

		// Indices should be 0, 1, 2, 3, 4
		assert_eq!(assigned_indices, vec![0, 1, 2, 3, 4]);
	});
}
