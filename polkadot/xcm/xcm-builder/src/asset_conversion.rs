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

//! Adapters to work with [`frame_support::traits::fungibles`] through XCM.

use core::{marker::PhantomData, result};
use frame_support::traits::{Contains, Get};
use sp_runtime::traits::MaybeEquivalence;
use xcm::latest::prelude::*;
use xcm_executor::traits::{
	Error as MatchError, MatchesFungibles, MatchesInstance, MatchesNonFungible, MatchesNonFungibles,
};

/// Converter struct implementing `AssetIdConversion` converting a numeric asset ID (must be
/// `TryFrom/TryInto<u128>`) into a `GeneralIndex` junction, prefixed by some `Location` value.
/// The `Location` value will typically be a `PalletInstance` junction.
pub struct AsPrefixedGeneralIndex<Prefix, AssetId, ConvertAssetId, L = Location>(
	PhantomData<(Prefix, AssetId, ConvertAssetId, L)>,
);
impl<
		Prefix: Get<L>,
		AssetId: Clone,
		ConvertAssetId: MaybeEquivalence<u128, AssetId>,
		L: TryInto<Location> + TryFrom<Location> + Clone,
	> MaybeEquivalence<L, AssetId> for AsPrefixedGeneralIndex<Prefix, AssetId, ConvertAssetId, L>
{
	fn convert(id: &L) -> Option<AssetId> {
		let prefix = Prefix::get();
		let latest_prefix: Location = prefix.try_into().ok()?;
		let latest_id: Location = (*id).clone().try_into().ok()?;
		if latest_prefix.parent_count() != latest_id.parent_count() ||
			latest_prefix
				.interior()
				.iter()
				.enumerate()
				.any(|(index, junction)| latest_id.interior().at(index) != Some(junction))
		{
			return None;
		}
		match latest_id.interior().at(latest_prefix.interior().len()) {
			Some(Junction::GeneralIndex(id)) => ConvertAssetId::convert(&id),
			_ => None,
		}
	}
	fn convert_back(what: &AssetId) -> Option<L> {
		let location = Prefix::get();
		let mut latest_location: Location = location.try_into().ok()?;
		let id = ConvertAssetId::convert_back(what)?;
		latest_location.push_interior(Junction::GeneralIndex(id)).ok()?;
		latest_location.try_into().ok()
	}
}

pub struct ConvertedConcreteId<AssetId, Balance, ConvertAssetId, ConvertOther>(
	PhantomData<(AssetId, Balance, ConvertAssetId, ConvertOther)>,
);
impl<
		AssetId: Clone,
		Balance: Clone,
		ConvertAssetId: MaybeEquivalence<Location, AssetId>,
		ConvertBalance: MaybeEquivalence<u128, Balance>,
	> MatchesFungibles<AssetId, Balance>
	for ConvertedConcreteId<AssetId, Balance, ConvertAssetId, ConvertBalance>
{
	fn matches_fungibles(a: &Asset) -> result::Result<(AssetId, Balance), MatchError> {
		let (amount, id) = match (&a.fun, &a.id) {
			(Fungible(ref amount), AssetId(ref id)) => (amount, id),
			_ => return Err(MatchError::AssetNotHandled),
		};
		let what = ConvertAssetId::convert(id).ok_or(MatchError::AssetIdConversionFailed)?;
		let amount =
			ConvertBalance::convert(amount).ok_or(MatchError::AmountToBalanceConversionFailed)?;
		Ok((what, amount))
	}
}
impl<
		ClassId: Clone,
		InstanceId: Clone,
		ConvertClassId: MaybeEquivalence<Location, ClassId>,
		ConvertInstanceId: MaybeEquivalence<AssetInstance, InstanceId>,
	> MatchesNonFungibles<ClassId, InstanceId>
	for ConvertedConcreteId<ClassId, InstanceId, ConvertClassId, ConvertInstanceId>
{
	fn matches_nonfungibles(a: &Asset) -> result::Result<(ClassId, InstanceId), MatchError> {
		let (instance, class) = match (&a.fun, &a.id) {
			(NonFungible(ref instance), AssetId(ref class)) => (instance, class),
			_ => return Err(MatchError::AssetNotHandled),
		};
		let what = ConvertClassId::convert(class).ok_or(MatchError::AssetIdConversionFailed)?;
		let instance =
			ConvertInstanceId::convert(instance).ok_or(MatchError::InstanceConversionFailed)?;
		Ok((what, instance))
	}
}

pub struct MatchedConvertedConcreteId<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertOther>(
	PhantomData<(AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertOther)>,
);
impl<
		AssetId: Clone,
		Balance: Clone,
		MatchAssetId: Contains<Location>,
		ConvertAssetId: MaybeEquivalence<Location, AssetId>,
		ConvertBalance: MaybeEquivalence<u128, Balance>,
	> MatchesFungibles<AssetId, Balance>
	for MatchedConvertedConcreteId<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertBalance>
{
	fn matches_fungibles(a: &Asset) -> result::Result<(AssetId, Balance), MatchError> {
		let (amount, id) = match (&a.fun, &a.id) {
			(Fungible(ref amount), AssetId(ref id)) if MatchAssetId::contains(id) => (amount, id),
			_ => return Err(MatchError::AssetNotHandled),
		};
		let what = ConvertAssetId::convert(id).ok_or(MatchError::AssetIdConversionFailed)?;
		let amount =
			ConvertBalance::convert(amount).ok_or(MatchError::AmountToBalanceConversionFailed)?;
		Ok((what, amount))
	}
}
impl<
		ClassId: Clone,
		InstanceId: Clone,
		MatchClassId: Contains<Location>,
		ConvertClassId: MaybeEquivalence<Location, ClassId>,
		ConvertInstanceId: MaybeEquivalence<AssetInstance, InstanceId>,
	> MatchesNonFungibles<ClassId, InstanceId>
	for MatchedConvertedConcreteId<
		ClassId,
		InstanceId,
		MatchClassId,
		ConvertClassId,
		ConvertInstanceId,
	>
{
	fn matches_nonfungibles(a: &Asset) -> result::Result<(ClassId, InstanceId), MatchError> {
		let (instance, class) = match (&a.fun, &a.id) {
			(NonFungible(ref instance), AssetId(ref class)) if MatchClassId::contains(class) => {
				(instance, class)
			},
			_ => return Err(MatchError::AssetNotHandled),
		};
		let what = ConvertClassId::convert(class).ok_or(MatchError::AssetIdConversionFailed)?;
		let instance =
			ConvertInstanceId::convert(instance).ok_or(MatchError::InstanceConversionFailed)?;
		Ok((what, instance))
	}
}

/// An adapter that implements the unified unique instances matcher [`MatchesInstance`] trait
/// for the [`MatchesNonFungibles`].
/// The resulting matcher expects the instances to be part of some class (i.e., instance group,
/// such as an NFT collection).
///
/// * `ClassId` is the ID of an instance class (e.g., NFT collection ID),
/// * `InstanceId` is a class-scoped ID of a class member's unique instance (e.g., an NFT ID inside
///   a collection).
pub struct MatchInClassInstances<Matcher>(PhantomData<Matcher>);

impl<ClassId, InstanceId, Matcher: MatchesNonFungibles<ClassId, InstanceId>>
	MatchesInstance<(ClassId, InstanceId)> for MatchInClassInstances<Matcher>
{
	fn matches_instance(a: &Asset) -> result::Result<(ClassId, InstanceId), MatchError> {
		Matcher::matches_nonfungibles(a)
	}
}

/// An adapter that implements the unified unique instances matcher [`MatchesInstance`] trait
/// for the [`MatchesNonFungible`].
/// The resulting matcher expects the instances to be fully individual, not belonging to any group
/// (such as an NFT collection).
///
/// In practice, this typically means that the `InstanceId` is an indivisible ID (i.e., it is not
/// composed of multiple IDs).
pub struct MatchClasslessInstances<Matcher>(PhantomData<Matcher>);

impl<InstanceId, Matcher: MatchesNonFungible<InstanceId>> MatchesInstance<InstanceId>
	for MatchClasslessInstances<Matcher>
{
	fn matches_instance(a: &Asset) -> result::Result<InstanceId, MatchError> {
		Matcher::matches_nonfungible(a).ok_or(MatchError::AssetNotHandled)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	use sp_runtime::traits::TryConvertInto;

	struct OnlyParentZero;
	impl Contains<Location> for OnlyParentZero {
		fn contains(a: &Location) -> bool {
			match a {
				Location { parents: 0, .. } => true,
				_ => false,
			}
		}
	}

	#[test]
	fn matched_converted_concrete_id_for_fungibles_works() {
		type AssetIdForTrustBackedAssets = u32;
		type Balance = u128;
		frame_support::parameter_types! {
			pub TrustBackedAssetsPalletLocation: Location = PalletInstance(50).into();
		}

		// ConvertedConcreteId cfg
		type Converter = MatchedConvertedConcreteId<
			AssetIdForTrustBackedAssets,
			Balance,
			OnlyParentZero,
			AsPrefixedGeneralIndex<
				TrustBackedAssetsPalletLocation,
				AssetIdForTrustBackedAssets,
				TryConvertInto,
			>,
			TryConvertInto,
		>;
		assert_eq!(
			TrustBackedAssetsPalletLocation::get(),
			Location { parents: 0, interior: [PalletInstance(50)].into() }
		);

		// err - does not match
		assert_eq!(
			Converter::matches_fungibles(&Asset {
				id: AssetId(Location::new(1, [PalletInstance(50), GeneralIndex(1)])),
				fun: Fungible(12345),
			}),
			Err(MatchError::AssetNotHandled)
		);

		// err - matches, but convert fails
		assert_eq!(
			Converter::matches_fungibles(&Asset {
				id: AssetId(Location::new(
					0,
					[PalletInstance(50), GeneralKey { length: 1, data: [1; 32] }]
				)),
				fun: Fungible(12345),
			}),
			Err(MatchError::AssetIdConversionFailed)
		);

		// err - matches, but NonFungible
		assert_eq!(
			Converter::matches_fungibles(&Asset {
				id: AssetId(Location::new(0, [PalletInstance(50), GeneralIndex(1)])),
				fun: NonFungible(Index(54321)),
			}),
			Err(MatchError::AssetNotHandled)
		);

		// ok
		assert_eq!(
			Converter::matches_fungibles(&Asset {
				id: AssetId(Location::new(0, [PalletInstance(50), GeneralIndex(1)])),
				fun: Fungible(12345),
			}),
			Ok((1, 12345))
		);
	}

	#[test]
	fn matched_converted_concrete_id_for_nonfungibles_works() {
		type ClassId = u32;
		type ClassInstanceId = u64;
		frame_support::parameter_types! {
			pub TrustBackedAssetsPalletLocation: Location = PalletInstance(50).into();
		}

		// ConvertedConcreteId cfg
		struct ClassInstanceIdConverter;
		impl MaybeEquivalence<AssetInstance, ClassInstanceId> for ClassInstanceIdConverter {
			fn convert(value: &AssetInstance) -> Option<ClassInstanceId> {
				(*value).try_into().ok()
			}

			fn convert_back(value: &ClassInstanceId) -> Option<AssetInstance> {
				Some(AssetInstance::from(*value))
			}
		}

		type Converter = MatchedConvertedConcreteId<
			ClassId,
			ClassInstanceId,
			OnlyParentZero,
			AsPrefixedGeneralIndex<TrustBackedAssetsPalletLocation, ClassId, TryConvertInto>,
			ClassInstanceIdConverter,
		>;
		assert_eq!(
			TrustBackedAssetsPalletLocation::get(),
			Location { parents: 0, interior: [PalletInstance(50)].into() }
		);

		// err - does not match
		assert_eq!(
			Converter::matches_nonfungibles(&Asset {
				id: AssetId(Location::new(1, [PalletInstance(50), GeneralIndex(1)])),
				fun: NonFungible(Index(54321)),
			}),
			Err(MatchError::AssetNotHandled)
		);

		// err - matches, but convert fails
		assert_eq!(
			Converter::matches_nonfungibles(&Asset {
				id: AssetId(Location::new(
					0,
					[PalletInstance(50), GeneralKey { length: 1, data: [1; 32] }]
				)),
				fun: NonFungible(Index(54321)),
			}),
			Err(MatchError::AssetIdConversionFailed)
		);

		// err - matches, but Fungible vs NonFungible
		assert_eq!(
			Converter::matches_nonfungibles(&Asset {
				id: AssetId(Location::new(0, [PalletInstance(50), GeneralIndex(1)])),
				fun: Fungible(12345),
			}),
			Err(MatchError::AssetNotHandled)
		);

		// ok
		assert_eq!(
			Converter::matches_nonfungibles(&Asset {
				id: AssetId(Location::new(0, [PalletInstance(50), GeneralIndex(1)])),
				fun: NonFungible(Index(54321)),
			}),
			Ok((1, 54321))
		);
	}
}
