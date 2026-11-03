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

//! Remote integration tests for pallet-psm.
//!
//! These tests fetch live chain state (e.g., Asset Hub Westend) via RPC and execute
//! PSM operations against real asset data. Since the PSM pallet may not be deployed
//! on the live chain yet, the tests inject PSM configuration into the fetched state.

use frame_support::{
	assert_noop, assert_ok,
	traits::fungibles::{
		metadata::{Inspect as FungiblesMetadataInspect, Mutate as FungiblesMetadataMutate},
		Create as FungiblesCreate, Inspect as FungiblesInspect, Mutate as FungiblesMutate,
	},
};
use remote_externalities::{Builder, Mode, OfflineConfig, OnlineConfig, SnapshotConfig};
use sp_runtime::{
	traits::{AccountIdConversion, Block as BlockT, Zero},
	DeserializeOwned, Saturating,
};

pub const LOG_TARGET: &str = "runtime::psm::remote-tests";

/// Balance type used by the PSM pallet's fungibles.
type BalanceOf<Runtime> =
	<<Runtime as pallet_psm::Config>::Fungibles as frame_support::traits::fungibles::Inspect<
		<Runtime as frame_system::Config>::AccountId,
	>>::Balance;

/// Balance type used by the PSM pallet's fungibles.
pub type AssetIdOf<Runtime> = <Runtime as pallet_psm::Config>::AssetId;

/// [`PsmTestConfig`] for a given runtime.
pub type PsmTestConfigOf<Runtime> = PsmTestConfig<
	AssetIdOf<Runtime>,
	<Runtime as frame_system::Config>::AccountId,
	BalanceOf<Runtime>,
>;

/// Configuration for which asset to use as the external asset in tests.
pub struct PsmTestConfig<AssetId, AccountId, Balance> {
	/// The internal stablecoin asset ID for the PSM instance under test.
	pub internal_asset_id: AssetId,
	/// The external asset asset ID.
	pub external_asset_id: AssetId,
	/// The expected decimal precision for the internal asset (e.g., 6).
	pub internal_asset_decimals: u8,
	/// PSM fee destination written into the bootstrapped `PsmInfo`.
	pub fee_destination: AccountId,
	/// Absolute internal-asset debt ceiling for the PSM instance.
	pub max_debt: Balance,
	/// Minting fee for the `(internal_asset, external_asset)` pair.
	pub minting_fee: sp_runtime::Permill,
	/// Redemption fee for the `(internal_asset, external_asset)` pair.
	pub redemption_fee: sp_runtime::Permill,
	/// Ceiling weight assigned to `external_asset` on the PSM.
	pub ceiling_weight: sp_runtime::Permill,
	/// The pallet name for the assets pallet on the target chain (e.g., "Assets").
	/// Used to determine which storage prefixes to fetch from the live chain.
	pub assets_pallet_name: String,
	/// Optional setup callback invoked before creating the internal asset.
	/// Use this to set `NextAssetId` so that the asset can be created with
	/// the desired ID on chains that use `AutoIncAssetId`.
	pub pre_create_hook: Option<Box<dyn Fn()>>,
}

/// Amount of external asset to swap in tests (1000 units).
const SWAP_AMOUNT: u128 = 1_000;
/// Amount of external asset to fund the test caller with (2000 units).
const FUND_AMOUNT: u128 = 2_000;
/// Amount for a small redeem in circuit breaker tests (100 units).
const SMALL_REDEEM: u128 = 100;

/// Common test state returned by [`setup`].
struct TestEnv<Runtime: pallet_psm::Config + frame_system::Config> {
	internal_asset_id: Runtime::AssetId,
	asset_id: Runtime::AssetId,
	caller: Runtime::AccountId,
	psm_account: Runtime::AccountId,
	swap_amount: BalanceOf<Runtime>,
}

/// Create internal asset if needed, configure PSM, and fund test accounts.
/// Must be called inside `execute_with`.
fn setup<Runtime>(config: &PsmTestConfigOf<Runtime>) -> TestEnv<Runtime>
where
	Runtime: pallet_psm::Config + frame_system::Config,
	BalanceOf<Runtime>: TryFrom<u128> + core::fmt::Debug,
	Runtime::Fungibles:
		FungiblesCreate<Runtime::AccountId> + FungiblesMetadataMutate<Runtime::AccountId>,
{
	let asset_id = config.external_asset_id.clone();
	let internal_asset_id = config.internal_asset_id.clone();
	let psm_account: Runtime::AccountId =
		pallet_psm::Pallet::<Runtime>::psm_account(&internal_asset_id);

	// Check that the external asset actually exists on-chain.
	assert!(
		<Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::asset_exists(
			asset_id.clone()
		),
		"External asset does not exist on the live chain. \
		 Make sure the asset ID is correct."
	);

	let decimals = <Runtime::Fungibles as FungiblesMetadataInspect<Runtime::AccountId>>::decimals(
		asset_id.clone(),
	);
	log::info!(
		target: LOG_TARGET,
		"External asset found with {} decimals",
		decimals,
	);

	// Create the internal asset if it doesn't exist yet.
	if !<Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::asset_exists(
		internal_asset_id.clone(),
	) {
		// Run pre-create hook (e.g., set NextAssetId for AutoIncAssetId chains).
		if let Some(hook) = &config.pre_create_hook {
			hook();
		}

		let _ = frame_system::Pallet::<Runtime>::inc_providers(&psm_account);

		assert_ok!(<Runtime::Fungibles as FungiblesCreate<Runtime::AccountId>>::create(
			internal_asset_id.clone(),
			psm_account.clone(),
			true,
			10_000u128.try_into().unwrap_or_else(|_| panic!("balance conversion failed")),
		));

		// Set internal asset metadata using the configured decimals.
		assert_ok!(<Runtime::Fungibles as FungiblesMetadataMutate<Runtime::AccountId>>::set(
			internal_asset_id.clone(),
			&psm_account,
			b"internal".to_vec(),
			b"internal".to_vec(),
			config.internal_asset_decimals,
		));

		log::info!(
			target: LOG_TARGET,
			"Created internal asset with {} decimals",
			config.internal_asset_decimals,
		);
	}

	// Verify the stable asset and external asset have matching decimals.
	let internal_decimals =
		<Runtime::Fungibles as FungiblesMetadataInspect<Runtime::AccountId>>::decimals(
			internal_asset_id.clone(),
		);
	let external_decimals =
		<Runtime::Fungibles as FungiblesMetadataInspect<Runtime::AccountId>>::decimals(
			asset_id.clone(),
		);
	assert_eq!(
		internal_decimals, external_decimals,
		"Decimals mismatch: internal={} vs external={}",
		internal_decimals, external_decimals,
	);

	// Bootstrap the PSM by writing the [`PsmInfo`] / [`PsmAdminInfo`] records directly with
	// `Root` as both admins, then setting up the external via the public dispatchables
	// (dispatched as root, which matches `full_admin`). This avoids needing to fund a
	// signer for `create_psm` in the remote-ext environment.
	let internal_decimals_u8 = internal_decimals;
	let root_origin: <Runtime as pallet_psm::Config>::PalletsOrigin =
		frame_system::RawOrigin::<Runtime::AccountId>::Root.into();
	pallet_psm::Psm::<Runtime>::insert(
		&internal_asset_id,
		pallet_psm::PsmInfo::<Runtime> {
			fee_destination: config.fee_destination.clone(),
			max_debt: config.max_debt,
			min_swap_amount: 1u128
				.try_into()
				.unwrap_or_else(|_| panic!("balance conversion failed")),
			internal_decimals: internal_decimals_u8,
			external_count: 0,
		},
	);
	pallet_psm::PsmAdmin::<Runtime>::insert(
		&internal_asset_id,
		pallet_psm::PsmAdminInfo::<Runtime> {
			full_admin: root_origin.clone(),
			emergency_admin: root_origin,
			deposit: None,
		},
	);
	let psm_account_id = pallet_psm::Pallet::<Runtime>::psm_account(&internal_asset_id);
	if !frame_system::Pallet::<Runtime>::account_exists(&psm_account_id) {
		let _ = frame_system::Pallet::<Runtime>::inc_providers(&psm_account_id);
	}
	if !frame_system::Pallet::<Runtime>::account_exists(&config.fee_destination) {
		let _ = frame_system::Pallet::<Runtime>::inc_providers(&config.fee_destination);
	}
	assert_ok!(pallet_psm::Pallet::<Runtime>::add_external_asset(
		frame_system::RawOrigin::Root.into(),
		internal_asset_id.clone(),
		asset_id.clone(),
	));
	assert_ok!(pallet_psm::Pallet::<Runtime>::set_minting_fee(
		frame_system::RawOrigin::Root.into(),
		internal_asset_id.clone(),
		asset_id.clone(),
		config.minting_fee,
	));
	assert_ok!(pallet_psm::Pallet::<Runtime>::set_redemption_fee(
		frame_system::RawOrigin::Root.into(),
		internal_asset_id.clone(),
		asset_id.clone(),
		config.redemption_fee,
	));
	assert_ok!(pallet_psm::Pallet::<Runtime>::set_asset_ceiling_weight(
		frame_system::RawOrigin::Root.into(),
		internal_asset_id.clone(),
		asset_id.clone(),
		config.ceiling_weight,
	));

	// Fund test account.
	let caller: Runtime::AccountId =
		frame_support::PalletId(*b"py/test!").into_account_truncating();
	let _ = frame_system::Pallet::<Runtime>::inc_providers(&caller);

	let unit = 10u128.pow(config.internal_asset_decimals as u32);

	let fund_amount: BalanceOf<Runtime> = (FUND_AMOUNT * unit)
		.try_into()
		.unwrap_or_else(|_| panic!("balance conversion failed"));
	assert_ok!(<Runtime::Fungibles as FungiblesMutate<Runtime::AccountId>>::mint_into(
		asset_id.clone(),
		&caller,
		fund_amount,
	));

	let swap_amount: BalanceOf<Runtime> = (SWAP_AMOUNT * unit)
		.try_into()
		.unwrap_or_else(|_| panic!("balance conversion failed"));

	TestEnv { internal_asset_id, asset_id, caller, psm_account, swap_amount }
}

const SNAPSHOT_PATH: &str = "psm_remote_test.snap";

/// Build remote externalities by fetching live chain state.
///
/// State is fetched from the RPC node and cached to a local snapshot file so
/// that multiple tests within the same run can reuse it without extra RPC calls.
///
/// Call [`clear_ext`] after all tests complete to remove the snapshot file.
pub async fn build_ext<Block>(
	ws_url: String,
	assets_pallet_name: String,
) -> remote_externalities::RemoteExternalities<Block>
where
	Block: BlockT + DeserializeOwned,
	Block::Header: DeserializeOwned,
{
	Builder::<Block>::new()
		.mode(Mode::OfflineOrElseOnline(
			OfflineConfig { state_snapshot: SnapshotConfig::new(SNAPSHOT_PATH) },
			OnlineConfig {
				transport_uris: vec![ws_url],
				pallets: vec![assets_pallet_name],
				state_snapshot: Some(SnapshotConfig::new(SNAPSHOT_PATH)),
				..Default::default()
			},
		))
		.build()
		.await
		.unwrap()
}

/// Remove the snapshot file so the next run fetches fresh state.
pub fn clear_ext() {
	let _ = std::fs::remove_file(SNAPSHOT_PATH);
}

/// Test that minting and redeeming through the PSM works against real on-chain
/// asset data.
///
/// This test:
/// 1. Sets up PSM with an approved external asset
/// 2. Mints internal asset by depositing the external asset
/// 3. Redeems internal asset back for the external asset
/// 4. Verifies balances, debt tracking, and fee accounting
pub fn mint_and_redeem<Runtime, Block>(
	ext: &mut remote_externalities::RemoteExternalities<Block>,
	config: &PsmTestConfigOf<Runtime>,
) where
	Runtime: pallet_psm::Config + frame_system::Config,
	Block: BlockT,
	BalanceOf<Runtime>: TryFrom<u128> + core::fmt::Debug,
	Runtime::Fungibles:
		FungiblesCreate<Runtime::AccountId> + FungiblesMetadataMutate<Runtime::AccountId>,
{
	ext.execute_with(|| {
		let TestEnv { internal_asset_id, asset_id, caller, psm_account, swap_amount } =
			setup::<Runtime>(config);

		let balance_before = <Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::balance(
			asset_id.clone(),
			&caller,
		);

		log::info!(
			target: LOG_TARGET,
			"Test account external asset balance: {:?}",
			balance_before,
		);

		// Test mint
		assert_ok!(pallet_psm::Pallet::<Runtime>::mint(
			frame_system::RawOrigin::Signed(caller.clone()).into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			swap_amount,
			config.minting_fee,
		));

		let balance_after_mint =
			<Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::balance(
				asset_id.clone(),
				&caller,
			);
		assert_eq!(
			balance_after_mint,
			balance_before - swap_amount,
			"Caller external balance should decrease by exactly swap_amount"
		);

		let total_debt = pallet_psm::PsmDebt::<Runtime>::iter_values()
			.fold(BalanceOf::<Runtime>::zero(), |acc, debt| acc.saturating_add(debt));
		assert_eq!(total_debt, swap_amount, "PSM total debt should equal the swap amount");

		// The PSM account should hold the external asset.
		let psm_external = <Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::balance(
			asset_id.clone(),
			&psm_account,
		);
		assert_eq!(psm_external, swap_amount, "PSM should hold the external asset");

		log::info!(
			target: LOG_TARGET,
			"Mint successful: debt={:?}, PSM external balance={:?}",
			total_debt,
			psm_external,
		);

		// Redeem all internal asset the caller has.
		let internal_balance =
			<Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::balance(
				internal_asset_id.clone(),
				&caller,
			);
		let redeem_amount = internal_balance;

		assert_ok!(pallet_psm::Pallet::<Runtime>::redeem(
			frame_system::RawOrigin::Signed(caller.clone()).into(),
			internal_asset_id.clone(),
			asset_id,
			redeem_amount,
			config.redemption_fee,
		));

		// Verify caller's internal asset was fully spent.
		let internal_after = <Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::balance(
			internal_asset_id.clone(),
			&caller,
		);
		assert_eq!(internal_after, Zero::zero(), "Caller should have no internal asset remaining");

		// Debt should decrease after redeem but not reach zero (fees keep some debt alive).
		let debt_after = pallet_psm::PsmDebt::<Runtime>::iter_values()
			.fold(BalanceOf::<Runtime>::zero(), |acc, debt| acc.saturating_add(debt));
		assert!(debt_after > Zero::zero(), "Some debt should remain (fee portion)");
		assert!(debt_after < total_debt, "Debt should decrease after redeem");

		// Fee destination should have received fees.
		let fee_dest = pallet_psm::Psm::<Runtime>::get(internal_asset_id.clone())
			.expect("PSM installed by setup")
			.fee_destination;
		let fee_balance = <Runtime::Fungibles as FungiblesInspect<Runtime::AccountId>>::balance(
			internal_asset_id,
			&fee_dest,
		);
		assert!(fee_balance > Zero::zero(), "Fee destination should have collected fees");

		log::info!(
			target: LOG_TARGET,
			"Redeem successful: debt_after={:?}, fee_balance={:?}",
			debt_after,
			fee_balance,
		);

		log::info!(target: LOG_TARGET, "mint_and_redeem passed.");
	});
}

/// Test the circuit breaker mechanism against live chain state.
///
/// This test:
/// 1. Sets up PSM with an approved external asset
/// 2. Activates circuit breaker to `MintingDisabled` — verifies mint fails, redeem works
/// 3. Activates circuit breaker to `AllDisabled` — verifies both mint and redeem fail
/// 4. Deactivates circuit breaker — verifies both operations resume
pub fn circuit_breaker<Runtime, Block>(
	ext: &mut remote_externalities::RemoteExternalities<Block>,
	config: &PsmTestConfigOf<Runtime>,
) where
	Runtime: pallet_psm::Config + frame_system::Config,
	Block: BlockT,
	BalanceOf<Runtime>: TryFrom<u128> + core::fmt::Debug,
	Runtime::Fungibles:
		FungiblesCreate<Runtime::AccountId> + FungiblesMetadataMutate<Runtime::AccountId>,
{
	ext.execute_with(|| {
		let TestEnv { internal_asset_id, asset_id, caller, swap_amount, .. } =
			setup::<Runtime>(config);

		// Mint some internal asset first so we have something to redeem later.
		assert_ok!(pallet_psm::Pallet::<Runtime>::mint(
			frame_system::RawOrigin::Signed(caller.clone()).into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			swap_amount,
			config.minting_fee,
		));

		let unit = 10u128.pow(config.internal_asset_decimals as u32);
		let small_redeem: BalanceOf<Runtime> = (SMALL_REDEEM * unit)
			.try_into()
			.unwrap_or_else(|_| panic!("balance conversion failed"));

		// Test: MintingDisabled. Mint fails, redeem still works
		assert_ok!(pallet_psm::Pallet::<Runtime>::set_asset_status(
			frame_system::RawOrigin::Root.into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			pallet_psm::CircuitBreakerLevel::MintingDisabled,
		));

		assert_noop!(
			pallet_psm::Pallet::<Runtime>::mint(
				frame_system::RawOrigin::Signed(caller.clone()).into(),
				internal_asset_id.clone(),
				asset_id.clone(),
				swap_amount,
				config.minting_fee,
			),
			pallet_psm::Error::<Runtime>::MintingStopped
		);

		assert_ok!(pallet_psm::Pallet::<Runtime>::redeem(
			frame_system::RawOrigin::Signed(caller.clone()).into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			small_redeem,
			config.redemption_fee,
		));

		log::info!(target: LOG_TARGET, "MintingDisabled: mint blocked, redeem allowed");

		// Test: AllDisabled. Both mint and redeem fail
		assert_ok!(pallet_psm::Pallet::<Runtime>::set_asset_status(
			frame_system::RawOrigin::Root.into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			pallet_psm::CircuitBreakerLevel::AllDisabled,
		));

		assert_noop!(
			pallet_psm::Pallet::<Runtime>::mint(
				frame_system::RawOrigin::Signed(caller.clone()).into(),
				internal_asset_id.clone(),
				asset_id.clone(),
				swap_amount,
				config.minting_fee,
			),
			pallet_psm::Error::<Runtime>::MintingStopped
		);

		assert_noop!(
			pallet_psm::Pallet::<Runtime>::redeem(
				frame_system::RawOrigin::Signed(caller.clone()).into(),
				internal_asset_id.clone(),
				asset_id.clone(),
				small_redeem,
				config.redemption_fee,
			),
			pallet_psm::Error::<Runtime>::AllSwapsStopped
		);

		log::info!(target: LOG_TARGET, "AllDisabled: both mint and redeem blocked");

		// Test: Re-enable. Both operations resume
		assert_ok!(pallet_psm::Pallet::<Runtime>::set_asset_status(
			frame_system::RawOrigin::Root.into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			pallet_psm::CircuitBreakerLevel::AllEnabled,
		));

		assert_ok!(pallet_psm::Pallet::<Runtime>::mint(
			frame_system::RawOrigin::Signed(caller.clone()).into(),
			internal_asset_id.clone(),
			asset_id.clone(),
			swap_amount,
			config.minting_fee,
		));

		assert_ok!(pallet_psm::Pallet::<Runtime>::redeem(
			frame_system::RawOrigin::Signed(caller.clone()).into(),
			internal_asset_id,
			asset_id,
			small_redeem,
			config.redemption_fee,
		));

		log::info!(target: LOG_TARGET, "AllEnabled: both mint and redeem resumed");

		log::info!(target: LOG_TARGET, "circuit_breaker passed.");
	});
}
