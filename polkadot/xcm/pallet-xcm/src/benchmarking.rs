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
use frame_benchmarking::v2::*;
use frame_support::{assert_ok, weights::Weight};
use frame_system::RawOrigin;
use xcm::{
	latest::{prelude::*, MAX_ITEMS_IN_ASSETS},
	MAX_INSTRUCTIONS_TO_DECODE,
};
use xcm_builder::EnsureDelivery;
use xcm_executor::traits::{FeeReason, WeightBounds};

type RuntimeOrigin<T> = <T as frame_system::Config>::RuntimeOrigin;

/// Upper bound for a caller-supplied XCM blob, in bytes.
///
/// Meant to be kept in sync with `pallet_revive::limits::CALLDATA_BYTES`, the largest input a
/// contract can hand to the XCM precompile.
const MAX_XCM_BLOB_BYTES: u32 = 128 * 1024;

/// Upper bound for the `weigh_message` benchmark's input, in bytes.
const MAX_WEIGHABLE_BLOB_BYTES: u32 = 8 * 1024;

/// Pallet we're benchmarking here.
pub struct Pallet<T: Config>(crate::Pallet<T>);

/// Trait that must be implemented by runtime to be able to benchmark pallet properly.
pub trait Config: crate::Config + pallet_balances::Config {
	/// Helper that ensures successful delivery for extrinsics/benchmarks which need `SendXcm`.
	type DeliveryHelper: EnsureDelivery;

	/// A `Location` that can be reached via `XcmRouter`. Used only in benchmarks.
	///
	/// If `None`, the benchmarks that depend on a reachable destination will be skipped.
	fn reachable_dest() -> Option<Location> {
		None
	}

	/// A `(Asset, Location)` pair representing asset and the destination it can be
	/// teleported to. Used only in benchmarks.
	///
	/// Implementation should also make sure `dest` is reachable/connected.
	///
	/// If `None`, the benchmarks that depend on this will default to `Weight::MAX`.
	fn teleportable_asset_and_dest() -> Option<(Asset, Location)> {
		None
	}

	/// A `(Asset, Location)` pair representing asset and the destination it can be
	/// reserve-transferred to. Used only in benchmarks.
	///
	/// Implementation should also make sure `dest` is reachable/connected.
	///
	/// If `None`, the benchmarks that depend on this will default to `Weight::MAX`.
	fn reserve_transferable_asset_and_dest() -> Option<(Asset, Location)> {
		None
	}

	/// Sets up a complex transfer (usually consisting of a teleport and reserve-based transfer), so
	/// that runtime can properly benchmark `transfer_assets()` extrinsic. Should return a tuple
	/// `(Asset, u32, Location, dyn FnOnce())` representing the assets to transfer, the
	/// `u32` index of the asset to be used for fees, the destination chain for the transfer, and a
	/// `verify()` closure to verify the intended transfer side-effects.
	///
	/// Implementation should make sure the provided assets can be transacted by the runtime, there
	/// are enough balances in the involved accounts, and that `dest` is reachable/connected.
	///
	/// Used only in benchmarks.
	///
	/// If `None`, the benchmarks that depend on this will default to `Weight::MAX`.
	fn set_up_complex_asset_transfer() -> Option<(Assets, u32, Location, Box<dyn FnOnce()>)> {
		None
	}

	/// Gets an asset that can be handled by the AssetTransactor.
	///
	/// Used only in benchmarks.
	///
	/// Used, for example, in the benchmark for `claim_assets`.
	fn get_asset() -> Asset;

	/// Gets `n` distinct fungible assets handled by the `AssetTransactor`, preferring the most
	/// expensive-to-deposit kind the chain supports. Used only in benchmarks.
	///
	/// The default ignores `n` and returns a single asset, so it measures a ~zero per-asset
	/// slope: that is sound only for chains whose `AssetTransactor` handles one asset kind.
	/// Chains that can deposit several MUST override it, otherwise `claim_assets` is
	/// under-weighted; there is no generic default, since which ids are depositable is
	/// runtime-specific.
	fn get_assets(_n: u32) -> Assets {
		Self::get_asset().into()
	}

	/// Wraps `calls` into a single call that dispatches all of them, e.g. `utility.batch`. Used
	/// only by [`helpers::worst_case_weighable_message`].
	///
	/// If `None`, that worst case degrades to a single call and measures a far cheaper slope,
	/// so chains with a batching pallet MUST implement this or the precompile that charges
	/// `weigh_message` is under-charged.
	fn batch_call(
		_calls: Vec<<Self as crate::Config>::RuntimeCall>,
	) -> Option<<Self as crate::Config>::RuntimeCall> {
		None
	}
}

// The `From<Call<T>>` bound lets `weigh_message` build its own worst-case payload; every
// `construct_runtime!` runtime provides that conversion.
#[benchmarks(where <T as crate::Config>::RuntimeCall: From<crate::Call<T>>)]
mod benchmarks {
	use super::*;

	#[benchmark]
	fn send() -> Result<(), BenchmarkError> {
		let send_origin =
			T::SendXcmOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		if T::SendXcmOrigin::try_origin(send_origin.clone()).is_err() {
			return Err(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)));
		}
		let msg = Xcm(vec![ClearOrigin]);
		let versioned_dest: VersionedLocation = T::reachable_dest()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?
			.into();
		let versioned_msg = VersionedXcm::from(msg);

		// Ensure that origin can send to destination
		// (e.g. setup delivery fees, ensure router setup, ...)
		T::DeliveryHelper::ensure_successful_delivery(
			&Default::default(),
			&versioned_dest.clone().try_into().unwrap(),
			FeeReason::ChargeFees,
		);

		#[extrinsic_call]
		_(send_origin as RuntimeOrigin<T>, Box::new(versioned_dest), Box::new(versioned_msg));

		Ok(())
	}

	#[benchmark]
	fn teleport_assets() -> Result<(), BenchmarkError> {
		let (asset, destination) = T::teleportable_asset_and_dest()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;

		let assets: Assets = asset.clone().into();

		let caller: T::AccountId = whitelisted_caller();
		let send_origin = RawOrigin::Signed(caller.clone());
		let origin_location = T::ExecuteXcmOrigin::try_origin(send_origin.clone().into())
			.map_err(|_| BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		if !T::XcmTeleportFilter::contains(&(origin_location.clone(), assets.clone().into_inner()))
		{
			return Err(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)));
		}

		// Ensure that origin can send to destination
		// (e.g. setup delivery fees, ensure router setup, ...)
		let (_, _) = T::DeliveryHelper::ensure_successful_delivery(
			&origin_location,
			&destination,
			FeeReason::ChargeFees,
		);

		match &asset.fun {
			Fungible(amount) => {
				// Add transferred_amount to origin
				let context =
					XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
				let asset_to_mint = Asset { fun: Fungible(*amount), id: asset.id.clone() };
				let holdings = <T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::mint_asset(
					&asset_to_mint,
					&context,
				)
				.map_err(|error| {
					tracing::error!("Fungible asset couldn't be minted, error: {:?}", error);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
				<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::deposit_asset(
					holdings,
					&origin_location,
					Some(&context),
				)
				.map_err(|error| {
					tracing::error!("Fungible asset couldn't be deposited, error: {:?}", error.1);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
			},
			NonFungible(_instance) => {
				let context =
					XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
				let holdings = <T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::mint_asset(
					&asset, &context,
				)
				.map_err(|error| {
					tracing::error!("Nonfungible asset couldn't be minted, error: {:?}", error);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
				<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::deposit_asset(
					holdings,
					&origin_location,
					Some(&context),
				)
				.map_err(|error| {
					tracing::error!(
						"Nonfungible asset couldn't be deposited, error: {:?}",
						error.1
					);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
			},
		};

		let recipient = [0u8; 32];
		let versioned_dest: VersionedLocation = destination.into();
		let versioned_beneficiary: VersionedLocation =
			AccountId32 { network: None, id: recipient.into() }.into();
		let versioned_assets: VersionedAssets = assets.into();

		#[extrinsic_call]
		_(
			send_origin,
			Box::new(versioned_dest),
			Box::new(versioned_beneficiary),
			Box::new(versioned_assets),
			0,
		);

		Ok(())
	}

	#[benchmark]
	fn reserve_transfer_assets() -> Result<(), BenchmarkError> {
		let (asset, destination) = T::reserve_transferable_asset_and_dest()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;

		let assets: Assets = asset.clone().into();

		let caller: T::AccountId = whitelisted_caller();
		let send_origin = RawOrigin::Signed(caller.clone());
		let origin_location = T::ExecuteXcmOrigin::try_origin(send_origin.clone().into())
			.map_err(|_| BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		if !T::XcmReserveTransferFilter::contains(&(
			origin_location.clone(),
			assets.clone().into_inner(),
		)) {
			return Err(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)));
		}

		// Ensure that origin can send to destination
		// (e.g. setup delivery fees, ensure router setup, ...)
		let (_, _) = T::DeliveryHelper::ensure_successful_delivery(
			&origin_location,
			&destination,
			FeeReason::ChargeFees,
		);

		match &asset.fun {
			Fungible(amount) => {
				// Add transferred_amount to origin
				let context =
					XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
				let asset_to_mint = Asset { fun: Fungible(*amount), id: asset.id.clone() };
				let holdings = <T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::mint_asset(
					&asset_to_mint,
					&context,
				)
				.map_err(|error| {
					tracing::error!("Fungible asset couldn't be minted, error: {:?}", error);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
				<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::deposit_asset(
					holdings,
					&origin_location,
					Some(&context),
				)
				.map_err(|error| {
					tracing::error!("Fungible asset couldn't be deposited, error: {:?}", error.1);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
			},
			NonFungible(_instance) => {
				let context =
					XcmContext { origin: None, message_id: XcmHash::default(), topic: None };
				let holdings = <T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::mint_asset(
					&asset, &context,
				)
				.map_err(|error| {
					tracing::error!("Nonfungible asset couldn't be minted, error: {:?}", error);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
				<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::deposit_asset(
					holdings,
					&origin_location,
					Some(&context),
				)
				.map_err(|error| {
					tracing::error!(
						"Nonfungible asset couldn't be deposited, error: {:?}",
						error.1
					);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?;
			},
		};

		let recipient = [0u8; 32];
		let versioned_dest: VersionedLocation = destination.clone().into();
		let versioned_beneficiary: VersionedLocation =
			AccountId32 { network: None, id: recipient.into() }.into();
		let versioned_assets: VersionedAssets = assets.into();

		#[extrinsic_call]
		_(
			send_origin,
			Box::new(versioned_dest),
			Box::new(versioned_beneficiary),
			Box::new(versioned_assets),
			0,
		);

		match &asset.fun {
			Fungible(amount) => {
				assert_ok!(<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::withdraw_asset(
					&Asset { fun: Fungible(*amount), id: asset.id },
					&destination,
					None,
				));
			},
			NonFungible(_instance) => {
				assert_ok!(<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::withdraw_asset(
					&asset,
					&destination,
					None,
				));
			},
		};

		Ok(())
	}

	#[benchmark]
	fn transfer_assets() -> Result<(), BenchmarkError> {
		let (assets, _fee_index, destination, verify_fn) = T::set_up_complex_asset_transfer()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		let caller: T::AccountId = whitelisted_caller();
		let send_origin = RawOrigin::Signed(caller.clone());
		let recipient = [0u8; 32];
		let versioned_dest: VersionedLocation = destination.into();
		let versioned_beneficiary: VersionedLocation =
			AccountId32 { network: None, id: recipient.into() }.into();
		let versioned_assets: VersionedAssets = assets.into();

		// Ensure that origin can send to destination
		// (e.g. setup delivery fees, ensure router setup, ...)
		T::DeliveryHelper::ensure_successful_delivery(
			&Default::default(),
			&versioned_dest.clone().try_into().unwrap(),
			FeeReason::ChargeFees,
		);

		#[extrinsic_call]
		_(
			send_origin,
			Box::new(versioned_dest),
			Box::new(versioned_beneficiary),
			Box::new(versioned_assets),
			0,
			WeightLimit::Unlimited,
		);

		// run provided verification function
		verify_fn();
		Ok(())
	}

	#[benchmark]
	fn execute() -> Result<(), BenchmarkError> {
		let execute_origin =
			T::ExecuteXcmOrigin::try_successful_origin().map_err(|_| BenchmarkError::Weightless)?;
		let origin_location = T::ExecuteXcmOrigin::try_origin(execute_origin.clone())
			.map_err(|_| BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		let msg = Xcm(vec![ClearOrigin]);
		if !T::XcmExecuteFilter::contains(&(origin_location, msg.clone())) {
			return Err(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)));
		}
		let versioned_msg = VersionedXcm::from(msg);

		#[extrinsic_call]
		_(execute_origin as RuntimeOrigin<T>, Box::new(versioned_msg), Weight::MAX);

		Ok(())
	}

	#[benchmark]
	fn force_xcm_version() -> Result<(), BenchmarkError> {
		let loc = T::reachable_dest()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		let xcm_version = 2;

		#[extrinsic_call]
		_(RawOrigin::Root, Box::new(loc), xcm_version);

		Ok(())
	}

	#[benchmark]
	fn force_default_xcm_version() {
		#[extrinsic_call]
		_(RawOrigin::Root, Some(2))
	}

	#[benchmark]
	fn force_subscribe_version_notify() -> Result<(), BenchmarkError> {
		let versioned_loc: VersionedLocation = T::reachable_dest()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?
			.into();

		// Ensure that origin can send to destination
		// (e.g. setup delivery fees, ensure router setup, ...)
		T::DeliveryHelper::ensure_successful_delivery(
			&Default::default(),
			&versioned_loc.clone().try_into().unwrap(),
			FeeReason::ChargeFees,
		);

		#[extrinsic_call]
		_(RawOrigin::Root, Box::new(versioned_loc));

		Ok(())
	}

	#[benchmark]
	fn force_unsubscribe_version_notify() -> Result<(), BenchmarkError> {
		let loc = T::reachable_dest()
			.ok_or(BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		let versioned_loc: VersionedLocation = loc.clone().into();

		// Ensure that origin can send to destination
		// (e.g. setup delivery fees, ensure router setup, ...)
		T::DeliveryHelper::ensure_successful_delivery(
			&Default::default(),
			&versioned_loc.clone().try_into().unwrap(),
			FeeReason::ChargeFees,
		);

		let _ = crate::Pallet::<T>::request_version_notify(loc);

		#[extrinsic_call]
		_(RawOrigin::Root, Box::new(versioned_loc));

		Ok(())
	}

	#[benchmark]
	fn force_suspension() {
		#[extrinsic_call]
		_(RawOrigin::Root, true)
	}

	#[benchmark]
	fn migrate_supported_version() {
		let old_version = XCM_VERSION - 1;
		let loc = VersionedLocation::from(Location::from(Parent));
		SupportedVersion::<T>::insert(old_version, loc, old_version);

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::MigrateSupportedVersion,
				Weight::zero(),
			);
		}
	}

	#[benchmark]
	fn migrate_version_notifiers() {
		let old_version = XCM_VERSION - 1;
		let loc = VersionedLocation::from(Location::from(Parent));
		VersionNotifiers::<T>::insert(old_version, loc, 0);

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::MigrateVersionNotifiers,
				Weight::zero(),
			);
		}
	}

	#[benchmark]
	fn already_notified_target() -> Result<(), BenchmarkError> {
		let loc = T::reachable_dest().ok_or(BenchmarkError::Override(
			BenchmarkResult::from_weight(T::DbWeight::get().reads(1)),
		))?;
		let loc = VersionedLocation::from(loc);
		let current_version = T::AdvertisedXcmVersion::get();
		VersionNotifyTargets::<T>::insert(
			current_version,
			loc,
			(0, Weight::zero(), current_version),
		);

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::NotifyCurrentTargets(None),
				Weight::zero(),
			);
		}

		Ok(())
	}

	#[benchmark]
	fn notify_current_targets() -> Result<(), BenchmarkError> {
		let loc = T::reachable_dest().ok_or(BenchmarkError::Override(
			BenchmarkResult::from_weight(T::DbWeight::get().reads_writes(1, 3)),
		))?;
		let loc = VersionedLocation::from(loc);
		let current_version = T::AdvertisedXcmVersion::get();
		let old_version = current_version - 1;
		VersionNotifyTargets::<T>::insert(current_version, loc, (0, Weight::zero(), old_version));

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::NotifyCurrentTargets(None),
				Weight::zero(),
			);
		}

		Ok(())
	}

	#[benchmark]
	fn notify_target_migration_fail() {
		let newer_xcm_version = xcm::prelude::XCM_VERSION;
		let older_xcm_version = newer_xcm_version - 1;
		let bad_location: Location = Plurality { id: BodyId::Unit, part: BodyPart::Voice }.into();
		let bad_location = VersionedLocation::from(bad_location)
			.into_version(older_xcm_version)
			.expect("Version conversion should work");
		let current_version = T::AdvertisedXcmVersion::get();
		VersionNotifyTargets::<T>::insert(
			current_version,
			bad_location,
			(0, Weight::zero(), current_version),
		);

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::MigrateAndNotifyOldTargets,
				Weight::zero(),
			);
		}
	}

	#[benchmark]
	fn migrate_version_notify_targets() {
		let current_version = T::AdvertisedXcmVersion::get();
		let old_version = current_version - 1;
		let loc = VersionedLocation::from(Location::from(Parent));
		VersionNotifyTargets::<T>::insert(old_version, loc, (0, Weight::zero(), current_version));

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::MigrateAndNotifyOldTargets,
				Weight::zero(),
			);
		}
	}

	#[benchmark]
	fn migrate_and_notify_old_targets() -> Result<(), BenchmarkError> {
		let loc = T::reachable_dest().ok_or(BenchmarkError::Override(
			BenchmarkResult::from_weight(T::DbWeight::get().reads_writes(1, 3)),
		))?;
		let loc = VersionedLocation::from(loc);
		let old_version = T::AdvertisedXcmVersion::get() - 1;
		VersionNotifyTargets::<T>::insert(old_version, loc, (0, Weight::zero(), old_version));

		#[block]
		{
			crate::Pallet::<T>::lazy_migration(
				VersionMigrationStage::MigrateAndNotifyOldTargets,
				Weight::zero(),
			);
		}

		Ok(())
	}

	#[benchmark]
	fn new_query() {
		let responder = Location::from(Parent);
		let timeout = 1u32.into();
		let match_querier = Location::from(Here);

		#[block]
		{
			crate::Pallet::<T>::new_query(responder, timeout, match_querier);
		}
	}

	#[benchmark]
	fn take_response() {
		let responder = Location::from(Parent);
		let timeout = 1u32.into();
		let match_querier = Location::from(Here);
		let query_id = crate::Pallet::<T>::new_query(responder, timeout, match_querier);
		let infos = (0..xcm::v3::MaxPalletsInfo::get())
			.map(|_| {
				PalletInfo::new(
					u32::MAX,
					(0..xcm::v3::MaxPalletNameLen::get())
						.map(|_| 97u8)
						.collect::<Vec<_>>()
						.try_into()
						.unwrap(),
					(0..xcm::v3::MaxPalletNameLen::get())
						.map(|_| 97u8)
						.collect::<Vec<_>>()
						.try_into()
						.unwrap(),
					u32::MAX,
					u32::MAX,
					u32::MAX,
				)
				.unwrap()
			})
			.collect::<Vec<_>>();
		crate::Pallet::<T>::expect_response(
			query_id,
			Response::PalletsInfo(infos.try_into().unwrap()),
		);

		#[block]
		{
			<crate::Pallet<T> as QueryHandler>::take_response(query_id);
		}
	}

	#[benchmark]
	fn claim_assets(n: Linear<1, { MAX_ITEMS_IN_ASSETS as u32 }>) -> Result<(), BenchmarkError> {
		let claim_origin = RawOrigin::Signed(whitelisted_caller());
		let claim_location = T::ExecuteXcmOrigin::try_origin(claim_origin.clone().into())
			.map_err(|_| BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX)))?;
		let assets = T::get_assets(n);
		if (assets.len() as u32) < n {
			tracing::warn!(
				target: "xcm::benchmarking::pallet_xcm::claim_assets",
				requested = n,
				distinct = assets.len(),
				"`get_assets` returned fewer distinct assets than requested; the weight will \
				 have a ~zero per-asset slope. Chains that can deposit multiple distinct \
				 assets must override `benchmarking::Config::get_assets`.",
			);
		}
		let context = XcmContext { origin: None, message_id: [0u8; 32], topic: None };
		// Trap all assets with a single `drop_assets` call: the trap is keyed on the hash
		// of the whole asset set, so the claim must match it exactly.
		let mut holding = AssetsInHolding::new();
		for asset in assets.inner() {
			let minted =
				<T::XcmExecutor as XcmAssetTransfers>::AssetTransactor::mint_asset(asset, &context)
					.map_err(|_| {
						BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
					})?;
			holding.subsume_assets(minted);
		}
		crate::Pallet::<T>::drop_assets(&claim_location, holding, &context);
		let versioned_assets = VersionedAssets::from(assets);

		#[extrinsic_call]
		_(
			claim_origin,
			Box::new(versioned_assets),
			Box::new(VersionedLocation::from(claim_location)),
		);

		Ok(())
	}

	#[benchmark]
	fn add_authorized_alias() -> Result<(), BenchmarkError> {
		let who: T::AccountId = whitelisted_caller();
		let origin = RawOrigin::Signed(who.clone());
		let origin_location: VersionedLocation =
			T::ExecuteXcmOrigin::try_origin(origin.clone().into())
				.map_err(|_| {
					tracing::error!(
						target: "xcm::benchmarking::pallet_xcm::add_authorized_alias",
						?origin,
						"try_origin failed",
					);
					BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
				})?
				.into();

		// Give some multiple of ED
		let balance = T::ExistentialDeposit::get() * 1000000u32.into();
		let _ =
			<pallet_balances::Pallet::<T> as frame_support::traits::Currency<_>>::make_free_balance_be(&who, balance);

		let mut existing_aliases = BoundedVec::<OriginAliaser, MaxAuthorizedAliases>::new();
		// prepopulate list with `max-1` aliases to benchmark worst case
		for i in 1..MaxAuthorizedAliases::get() {
			let alias =
				Location::new(1, [Parachain(i), AccountId32 { network: None, id: [42_u8; 32] }])
					.into();
			let aliaser = OriginAliaser { location: alias, expiry: None };
			existing_aliases.try_push(aliaser).unwrap()
		}
		let footprint = aliasers_footprint(existing_aliases.len());
		let ticket = TicketOf::<T>::new(&who, footprint).map_err(|e| {
			tracing::error!(
				target: "xcm::benchmarking::pallet_xcm::add_authorized_alias",
				?who,
				?footprint,
				error=?e,
				"could not create ticket",
			);
			BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX))
		})?;
		let entry = AuthorizedAliasesEntry { aliasers: existing_aliases, ticket };
		AuthorizedAliases::<T>::insert(&origin_location, entry);

		// now benchmark adding new alias
		let aliaser: VersionedLocation =
			Location::new(1, [Parachain(1234), AccountId32 { network: None, id: [42_u8; 32] }])
				.into();

		#[extrinsic_call]
		_(origin, Box::new(aliaser), None);

		Ok(())
	}

	#[benchmark]
	fn remove_authorized_alias() -> Result<(), BenchmarkError> {
		let who: T::AccountId = whitelisted_caller();
		let origin = RawOrigin::Signed(who.clone());
		let error = BenchmarkError::Override(BenchmarkResult::from_weight(Weight::MAX));
		let origin_location =
			T::ExecuteXcmOrigin::try_origin(origin.clone().into()).map_err(|_| {
				tracing::error!(
					target: "xcm::benchmarking::pallet_xcm::remove_authorized_alias",
					?origin,
					"try_origin failed",
				);
				error.clone()
			})?;
		// remove `network` from inner `AccountId32` for easier matching of automatic AccountId ->
		// Location conversions.
		let origin_location: VersionedLocation = match origin_location.unpack() {
			(0, [AccountId32 { network: _, id }]) => {
				Location::new(0, [AccountId32 { network: None, id: *id }]).into()
			},
			_ => {
				tracing::error!(
					target: "xcm::benchmarking::pallet_xcm::remove_authorized_alias",
					?origin_location,
					"unexpected origin failed",
				);
				return Err(error.clone());
			},
		};

		// Give some multiple of ED
		let balance = T::ExistentialDeposit::get() * 1000000u32.into();
		let _ =
			<pallet_balances::Pallet::<T> as frame_support::traits::Currency<_>>::make_free_balance_be(&who, balance);

		let mut existing_aliases = BoundedVec::<OriginAliaser, MaxAuthorizedAliases>::new();
		// prepopulate list with `max` aliases to benchmark worst case
		for i in 1..MaxAuthorizedAliases::get() + 1 {
			let alias =
				Location::new(1, [Parachain(i), AccountId32 { network: None, id: [42_u8; 32] }])
					.into();
			let aliaser = OriginAliaser { location: alias, expiry: None };
			existing_aliases.try_push(aliaser).unwrap()
		}
		let footprint = aliasers_footprint(existing_aliases.len());
		let ticket = TicketOf::<T>::new(&who, footprint).map_err(|e| {
			tracing::error!(
				target: "xcm::benchmarking::pallet_xcm::remove_authorized_alias",
				?who,
				?footprint,
				error=?e,
				"could not create ticket",
			);
			error
		})?;
		let entry = AuthorizedAliasesEntry { aliasers: existing_aliases, ticket };
		AuthorizedAliases::<T>::insert(&origin_location, entry);

		// now benchmark removing an alias
		let aliaser_to_remove: VersionedLocation =
			Location::new(1, [Parachain(1), AccountId32 { network: None, id: [42_u8; 32] }]).into();

		#[extrinsic_call]
		_(origin, Box::new(aliaser_to_remove));

		Ok(())
	}

	/// Decoding and weighing a caller-supplied message of `n` bytes.
	///
	/// The decode is measured, and goes through the precompile's entry point, because the
	/// precompile charges this weight before decoding.
	///
	/// `n` stops at [`MAX_WEIGHABLE_BLOB_BYTES`] rather than [`MAX_XCM_BLOB_BYTES`].
	#[benchmark]
	fn weigh_message(n: Linear<0, MAX_WEIGHABLE_BLOB_BYTES>) -> Result<(), BenchmarkError> {
		let bytes = helpers::worst_case_weighable_message::<T>(n);

		#[block]
		{
			let decoded =
				VersionedXcm::<<T as crate::Config>::RuntimeCall>::
					decode_all_with_mem_and_depth_limit(&mut &bytes[..])
					.expect("blob was just built by `worst_case_weighable_message`; qed");
			let mut message: Xcm<<T as crate::Config>::RuntimeCall> =
				decoded.try_into().expect("blob was built at the latest version; qed");
			// A message this large may legitimately exceed limits; finding that out is the cost
			// being measured.
			let _ = <T as crate::Config>::Weigher::weight(&mut message, Weight::MAX);
		}

		Ok(())
	}

	/// Only decoding, not weighing, a caller-supplied message of `n` bytes.
	#[benchmark]
	fn decode_xcm(n: Linear<0, MAX_XCM_BLOB_BYTES>) -> Result<(), BenchmarkError> {
		let bytes = helpers::worst_case_decodable_blob(n);

		#[block]
		{
			let _ = VersionedXcm::<()>::decode_all_with_mem_and_depth_limit(&mut &bytes[..]);
		}

		Ok(())
	}

	impl_benchmark_test_suite!(
		Pallet,
		crate::mock::new_test_ext_with_balances(Vec::new()),
		crate::mock::Test
	);
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::Test;

	/// A worst case that does not decode measures nothing, and the shared instruction budget
	/// makes that easy to trip by accident.
	#[test]
	fn worst_case_weighable_message_decodes() {
		for target_bytes in [0, 1, 1024, MAX_WEIGHABLE_BLOB_BYTES] {
			let bytes = helpers::worst_case_weighable_message::<Test>(target_bytes);
			assert!(
				bytes.len() >= target_bytes as usize,
				"undershooting the target would under-charge",
			);
			VersionedXcm::<<Test as crate::Config>::RuntimeCall>::decode_all_with_mem_and_depth_limit(
				&mut &bytes[..],
			)
			.expect("worst case must decode, otherwise the benchmark measures nothing");
		}
	}
}

pub mod helpers {
	use super::*;

	/// The worst case for `WeightInfo::weigh_message`: a `Transact` carrying
	/// `batch_call([pallet_xcm.execute(Xcm([])); N])`, encoded, of at least `target_bytes` bytes.
	///
	/// A `Transact` payload resolving to a local call is decoded eagerly, and weighing it recurses
	/// `get_dispatch_info()` over every batched call, so both costs scale with the number of calls
	/// the blob can hold. The worst case therefore spends `target_bytes` on as many calls as it
	/// fits, leaving the inner messages empty.
	///
	/// Empty also keeps the blob decodable: `MAX_INSTRUCTIONS_TO_DECODE` is one budget shared by
	/// every nesting level, so instructions inside a `Transact` spend the outer allowance.
	///
	/// Keep `target_bytes` at or below `MAX_WEIGHABLE_BLOB_BYTES`.
	pub fn worst_case_weighable_message<T: Config>(target_bytes: u32) -> Vec<u8>
	where
		<T as crate::Config>::RuntimeCall: From<crate::Call<T>>,
	{
		let inner = Xcm::<<T as crate::Config>::RuntimeCall>(Vec::new());
		let one: <T as crate::Config>::RuntimeCall = crate::Call::<T>::execute {
			message: Box::new(VersionedXcm::from(inner)),
			max_weight: Weight::zero(),
		}
		.into();
		let count = (target_bytes as usize).div_ceil(one.encode().len());

		let call = T::batch_call(vec![one.clone(); count]).unwrap_or_else(|| {
			tracing::warn!(
				target: "xcm::benchmarking::pallet_xcm::weigh_message",
				"`batch_call` is not implemented, so the measured per-byte slope is far below \
				 the real worst case. Chains with a batching pallet must implement \
				 `benchmarking::Config::batch_call`.",
			);
			one
		});

		let message = Xcm::<<T as crate::Config>::RuntimeCall>(vec![Transact {
			origin_kind: OriginKind::SovereignAccount,
			fallback_max_weight: None,
			call: call.encode().into(),
		}]);
		VersionedXcm::from(message).encode()
	}

	/// The worst case for `WeightInfo::decode_xcm`: an encoded `VersionedXcm<()>` of at least
	/// `target_bytes` bytes, as expensive to decode as the format allows.
	///
	/// `MAX_INSTRUCTIONS_TO_DECODE` caps the instruction count, so bytes past a hundred have to
	/// come from payloads. The densest decodable one is `ReserveAssetDeposited` carrying
	/// max-length asset ids, where nearly every byte is a nested enum or bounded-vec decode
	/// rather than a byte copy.
	pub fn worst_case_decodable_blob(target_bytes: u32) -> Vec<u8> {
		// `index` keeps the ids distinct so that `Assets` does not deduplicate them.
		let dense_asset = |index: u32| -> Asset {
			let mut data = [0u8; 32];
			data[..4].copy_from_slice(&index.to_le_bytes());
			Asset {
				id: AssetId(Location::new(1, [GeneralKey { length: 32, data }; 8])),
				fun: Fungible(u128::MAX),
			}
		};

		// Round up; undershooting `target_bytes` would under-charge.
		let asset_len = dense_asset(0).encode().len();
		let count = (target_bytes as usize).div_ceil(asset_len);
		debug_assert!(
			count <= MAX_INSTRUCTIONS_TO_DECODE as usize * MAX_ITEMS_IN_ASSETS,
			"more assets than the instruction limit can carry; the blob would not decode",
		);
		let assets = (0..count).map(|index| dense_asset(index as u32)).collect::<Vec<_>>();

		let instructions = assets
			.chunks(MAX_ITEMS_IN_ASSETS)
			.map(|chunk| ReserveAssetDeposited(chunk.to_vec().into()))
			.collect::<Vec<Instruction<()>>>();
		VersionedXcm::from(Xcm::<()>(instructions)).encode()
	}

	pub fn native_teleport_as_asset_transfer<T>(
		native_asset_location: Location,
		destination: Location,
	) -> Option<(Assets, u32, Location, Box<dyn FnOnce()>)>
	where
		T: Config + pallet_balances::Config,
		u128: From<<T as pallet_balances::Config>::Balance>,
	{
		// Relay/native token can be teleported to/from AH.
		let amount = T::ExistentialDeposit::get() * 100u32.into();
		let assets: Assets =
			Asset { fun: Fungible(amount.into()), id: AssetId(native_asset_location) }.into();
		let fee_index = 0u32;

		// Give some multiple of transferred amount
		let balance = amount * 10u32.into();
		let who = whitelisted_caller();
		let _ =
			<pallet_balances::Pallet::<T> as frame_support::traits::Currency<_>>::make_free_balance_be(&who, balance);
		// verify initial balance
		assert_eq!(pallet_balances::Pallet::<T>::free_balance(&who), balance);

		// verify transferred successfully
		let verify = Box::new(move || {
			// verify balance after transfer, decreased by transferred amount (and delivery fees)
			assert!(pallet_balances::Pallet::<T>::free_balance(&who) <= balance - amount);
		});
		Some((assets, fee_index, destination, verify))
	}
}
