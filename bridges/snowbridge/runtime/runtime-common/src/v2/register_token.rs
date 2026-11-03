// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: 2023 Snowfork <hello@snowfork.com>

use frame_support::{
	dispatch::RawOrigin,
	sp_runtime::traits::MaybeEquivalence,
	traits::{ContainsPair, EnsureOrigin, EnsureOriginWithArg, Everything, OriginTrait},
};
use frame_system::ensure_signed;
use pallet_xcm::{EnsureXcm, Origin as XcmOrigin};
use xcm::prelude::Location;

/// Origin check that verifies that an origin is the owner of a foreign asset.
/// 1. Allows XCM origins
/// 2. Checks that the asset exists
/// 3. The origin must be the owner of the asset
pub struct ForeignAssetOwner<IsForeign, AssetInspect, AccountId, LocationToAccountId, L = Location>(
	core::marker::PhantomData<(IsForeign, AssetInspect, AccountId, LocationToAccountId, L)>,
);

impl<
		IsForeign: ContainsPair<L, L>,
		AssetInspect: frame_support::traits::fungibles::roles::Inspect<AccountId>,
		AccountId: Eq + Clone,
		LocationToAccountId: xcm_executor::traits::ConvertLocation<AccountId>,
		RuntimeOrigin: From<XcmOrigin> + OriginTrait + Clone,
		L: From<Location> + Into<Location> + Clone,
	> EnsureOriginWithArg<RuntimeOrigin, L>
	for ForeignAssetOwner<IsForeign, AssetInspect, AccountId, LocationToAccountId, L>
where
	for<'a> &'a RuntimeOrigin::PalletsOrigin: TryInto<&'a XcmOrigin>,
	<AssetInspect as frame_support::traits::fungibles::Inspect<AccountId>>::AssetId: From<Location>,
{
	type Success = L;

	fn try_origin(
		origin: RuntimeOrigin,
		asset_location: &L,
	) -> Result<Self::Success, RuntimeOrigin> {
		let origin_location = EnsureXcm::<Everything, L>::try_origin(origin.clone())?;
		if !IsForeign::contains(asset_location, &origin_location) {
			return Err(origin);
		}
		let asset_location: Location = asset_location.clone().into();
		let owner = AssetInspect::owner(asset_location.into()).ok_or_else(|| origin.clone())?;
		let location: Location = origin_location.clone().into();
		let from =
			LocationToAccountId::convert_location(&location).ok_or_else(|| origin.clone())?;
		if from != owner {
			return Err(origin);
		}
		Ok(location.into())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin(a: &L) -> Result<RuntimeOrigin, ()> {
		let latest_location: Location = (*a).clone().try_into().map_err(|_| ())?;
		Ok(pallet_xcm::Origin::Xcm(latest_location).into())
	}
}

/// Origin check that verifies that an origin is the owner of a local trusted asset.
/// 1. Allows signed origins
/// 2. Checks that the asset exists
/// 3. The origin must be the owner of the asset
pub struct LocalAssetOwner<MatchAssetId, AssetInspect, AccountId, AssetId, L = Location>(
	core::marker::PhantomData<(MatchAssetId, AssetInspect, AccountId, AssetId, L)>,
);

impl<
		MatchAssetId: MaybeEquivalence<L, AssetId>,
		AssetInspect: frame_support::traits::fungibles::roles::Inspect<AccountId>,
		AccountId: Eq + Clone + Into<L>,
		AssetId: Eq + Clone,
		RuntimeOrigin: OriginTrait + Clone,
		L: From<Location> + Into<Location> + Clone,
	> EnsureOriginWithArg<RuntimeOrigin, L>
	for LocalAssetOwner<MatchAssetId, AssetInspect, AccountId, AssetId, L>
where
	RuntimeOrigin: Into<Result<RawOrigin<AccountId>, RuntimeOrigin>> + From<RawOrigin<AccountId>>,
	<AssetInspect as frame_support::traits::fungibles::Inspect<AccountId>>::AssetId: From<AssetId>,
{
	type Success = L;

	fn try_origin(
		origin: RuntimeOrigin,
		asset_location: &L,
	) -> Result<Self::Success, RuntimeOrigin> {
		let who = ensure_signed(origin.clone()).map_err(|_| origin.clone())?;
		let asset_id = MatchAssetId::convert(asset_location).ok_or_else(|| origin.clone())?;
		let owner = AssetInspect::owner(asset_id.into()).ok_or_else(|| origin.clone())?;
		if who != owner {
			return Err(origin);
		}
		Ok(who.into())
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn try_successful_origin(_: &L) -> Result<RuntimeOrigin, ()> {
		Ok(RawOrigin::Root.into())
	}
}
