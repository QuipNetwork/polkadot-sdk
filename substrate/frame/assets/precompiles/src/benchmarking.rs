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

//! Benchmarks for `pallet_assets_precompiles`.
//!
//! All benchmarks are registered under `foreign_assets::Pallet` so that a single
//! `frame-omni-bencher --pallet=pallet_assets_precompiles` run generates one
//! `weights.rs` containing every weight function.

#![cfg(feature = "runtime-benchmarks")]

use crate::{
	foreign_assets::pallet::{Config, Pallet},
	migration::MigrateForeignAssetPrecompileMappings,
};
use frame_benchmarking::v2::*;
use frame_support::{migrations::SteppedMigration, weights::WeightMeter};
use pallet_revive::precompiles::H160;
use sp_core::U256;
use sp_runtime::traits::StaticLookup;

/// Test owner address (Hardhat account #0: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266)
const TEST_OWNER: [u8; 20] = [
	0xf3, 0x9f, 0xd6, 0xe5, 0x1a, 0xad, 0x88, 0xf6, 0xf4, 0xce, 0x6a, 0xb8, 0x82, 0x72, 0x79, 0xcf,
	0xff, 0xb9, 0x22, 0x66,
];

fn test_verifying_contract() -> H160 {
	H160::from_low_u64_be(0x1234_5678)
}

fn test_owner() -> H160 {
	H160::from_slice(&TEST_OWNER)
}

/// Test token name for EIP-712 domain separator.
const TEST_TOKEN_NAME: &[u8] = b"Asset Permit";

#[benchmarks(
	where
		// Migration bounds
		T: pallet_assets::Config<T::AssetsInstance, AssetId = <T as Config>::ForeignAssetId>,
		// Permit bounds
		T: crate::permit::Config,
		<T as pallet_assets::Config<T::AssetsInstance>>::Balance: From<u32>,
		<T as pallet_assets::Config<T::AssetsInstance>>::AssetIdParameter: From<<T as pallet_assets::Config<T::AssetsInstance>>::AssetId>,
)]
mod benchmarks {
	use super::*;
	use pallet_assets::BenchmarkHelper;

	// ==================== Migration benchmarks ====================

	/// Benchmark one complete `step()` invocation of the
	/// [`MigrateForeignAssetPrecompileMappings`] stepped migration.
	#[benchmark]
	fn migrate_foreign_asset_step() {
		// Clear any pre-existing assets from genesis so that only our
		// benchmark asset is present during the migration step.
		let _ = pallet_assets::Asset::<T, T::AssetsInstance>::clear(u32::MAX, None);

		// Create one asset in pallet_assets storage.
		let caller: T::AccountId = whitelisted_caller();
		let caller_lookup = <T as frame_system::Config>::Lookup::unlookup(caller);
		let asset_id_param = <T as pallet_assets::Config<T::AssetsInstance>>::BenchmarkHelper::create_asset_id_parameter(42);
		let asset_id: <T as pallet_assets::Config<T::AssetsInstance>>::AssetId =
			asset_id_param.clone().into();

		pallet_assets::Pallet::<T, T::AssetsInstance>::force_create(
			frame_system::RawOrigin::Root.into(),
			asset_id_param,
			caller_lookup,
			true,
			1u32.into(),
		)
		.unwrap();

		// Remove the mapping that was auto-created by the AssetsCallback hook
		Pallet::<T>::remove_asset_mapping(&asset_id);

		// Verify no precompile mapping exists yet.
		assert!(Pallet::<T>::asset_index_of(&asset_id).is_none());

		let mut meter = WeightMeter::new();

		#[block]
		{
			MigrateForeignAssetPrecompileMappings::<T, T::AssetsInstance, ()>::step(
				None, &mut meter,
			)
			.unwrap();
		}

		// Verify the asset was migrated.
		assert!(Pallet::<T>::asset_index_of(&asset_id).is_some());
		// The step consumes the weight twice: once for migrating the asset and once for
		// discovering that there are no more assets to migrate.
		assert_eq!(
			meter.consumed(),
			<() as crate::weights::WeightInfo>::migrate_foreign_asset_step() * 2
		);
	}

	// ==================== Permit benchmarks ====================

	#[benchmark]
	fn nonces() {
		let verifying_contract = test_verifying_contract();
		let owner = test_owner();
		crate::permit::Nonces::<T>::insert(&verifying_contract, &owner, U256::from(42));

		let result;
		#[block]
		{
			result = crate::permit::Pallet::<T>::nonce(&verifying_contract, &owner);
		}
		assert_eq!(result, U256::from(42));
	}

	#[benchmark]
	fn domain_separator() {
		let verifying_contract = test_verifying_contract();
		let name = TEST_TOKEN_NAME;

		let result;
		#[block]
		{
			result =
				crate::permit::Pallet::<T>::compute_domain_separator(&verifying_contract, name);
		}
		assert_ne!(result, sp_core::H256::zero());
	}

	/// Benchmark for `use_permit` — the EIP-2612 signature verification and nonce
	/// increment that is called inside the `permit` precompile.
	///
	/// Measures: domain separator computation, struct hash, ECDSA recovery, nonce
	/// read + write. This is the weight charged for the cryptographic portion of
	/// a permit call (the asset approval weights are tracked separately).
	#[benchmark]
	fn use_permit() {
		let owner = test_owner();
		let spender = H160::from_low_u64_be(0x9876_5432);
		let verifying_contract = test_verifying_contract();
		let value: [u8; 32] = {
			let mut buf = [0u8; 32];
			buf[31] = 100; // value = 100
			buf
		};
		let deadline: [u8; 32] = {
			let mut buf = [0u8; 32];
			// Set deadline far in the future (max u64)
			buf[24..32].copy_from_slice(&u64::MAX.to_be_bytes());
			buf
		};

		// Set timestamp so deadline check passes.
		// Write directly to storage instead of calling `set_timestamp()`, which
		// triggers `OnTimestampSet` callbacks (e.g. pallet_babe slot validation)
		// that require additional setup not relevant to this benchmark.
		let timestamp: <T as pallet_timestamp::Config>::Moment =
			1_704_067_200_000u64.try_into().unwrap_or_default();
		pallet_timestamp::Now::<T>::put(timestamp);

		// Compute EIP-712 digest using runtime's chain_id.
		let nonce = U256::zero();
		let digest = crate::permit::Pallet::<T>::permit_digest(
			&verifying_contract,
			TEST_TOKEN_NAME,
			&owner,
			&spender,
			&value,
			&nonce,
			&deadline,
		);

		// Sign with Hardhat account #0 private key via sp_io host function.
		// This works in both native and WASM benchmark environments, unlike
		// using k256 directly which may not work in the WASM sandbox.
		let key_type = sp_core::crypto::KeyTypeId(*b"prmt");
		let pub_key = sp_io::crypto::ecdsa_generate(
			key_type,
			Some(b"0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_vec()),
		);
		let sig = sp_io::crypto::ecdsa_sign_prehashed(key_type, &pub_key, &digest)
			.expect("signing with Hardhat #0 key must succeed; qed");
		let sig_bytes: &[u8; 65] = sig.as_ref();
		let r: [u8; 32] = sig_bytes[0..32].try_into().expect("r is 32 bytes");
		let s: [u8; 32] = sig_bytes[32..64].try_into().expect("s is 32 bytes");
		let v: u8 = sig_bytes[64] + 27;

		#[block]
		{
			crate::permit::Pallet::<T>::use_permit(
				&verifying_contract,
				TEST_TOKEN_NAME,
				&owner,
				&spender,
				&value,
				&deadline,
				v,
				&r,
				&s,
			)
			.expect("use_permit should succeed");
		}

		// Verify nonce was incremented, confirming the full flow ran.
		assert_eq!(crate::permit::Pallet::<T>::nonce(&verifying_contract, &owner), U256::one());
	}

	impl_benchmark_test_suite!(Pallet, crate::mock::new_test_ext(), crate::mock::Test);
}
