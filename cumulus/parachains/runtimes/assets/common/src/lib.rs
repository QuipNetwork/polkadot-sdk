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

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarks;
mod erc20_transactor;
pub mod foreign_creators;
pub mod fungible_conversion;
pub mod local_and_foreign_assets;
pub mod matching;
pub mod migrations;
pub mod runtime_api;

pub use erc20_transactor::ERC20Transactor;

extern crate alloc;
extern crate core;

use crate::matching::{LocalLocationPattern, ParentLocation};
use alloc::vec::Vec;
use codec::{Decode, EncodeLike};
use core::{cmp::PartialEq, marker::PhantomData};
use frame_support::traits::{Contains, Equals, EverythingBut};
use parachains_common::{AssetIdForTrustBackedAssets, CollectionId, ItemId};
use sp_core::H160;
use sp_runtime::traits::{MaybeEquivalence, TryConvertInto};
use xcm::prelude::*;
use xcm_builder::{
	AsPrefixedGeneralIndex, MatchedConvertedConcreteId, StartsWith, WithLatestLocationConverter,
};

/// `Location` vs `AssetIdForTrustBackedAssets` converter for `TrustBackedAssets`
pub type AssetIdForTrustBackedAssetsConvert<TrustBackedAssetsPalletLocation, L = Location> =
	AsPrefixedGeneralIndex<
		TrustBackedAssetsPalletLocation,
		AssetIdForTrustBackedAssets,
		TryConvertInto,
		L,
	>;

/// `Location` vs `CollectionId` converter for `Uniques`
pub type CollectionIdForUniquesConvert<UniquesPalletLocation> =
	AsPrefixedGeneralIndex<UniquesPalletLocation, CollectionId, TryConvertInto>;

/// [`MatchedConvertedConcreteId`] converter dedicated for `TrustBackedAssets`
pub type TrustBackedAssetsConvertedConcreteId<
	TrustBackedAssetsPalletLocation,
	Balance,
	L = Location,
> = MatchedConvertedConcreteId<
	AssetIdForTrustBackedAssets,
	Balance,
	StartsWith<TrustBackedAssetsPalletLocation>,
	AssetIdForTrustBackedAssetsConvert<TrustBackedAssetsPalletLocation, L>,
	TryConvertInto,
>;

/// [`MatchedConvertedConcreteId`] converter dedicated for `Uniques`
pub type UniquesConvertedConcreteId<UniquesPalletLocation> = MatchedConvertedConcreteId<
	CollectionId,
	ItemId,
	// The asset starts with the uniques pallet. The `CollectionId` of the asset is specified as a
	// junction within the pallet itself.
	StartsWith<UniquesPalletLocation>,
	CollectionIdForUniquesConvert<UniquesPalletLocation>,
	TryConvertInto,
>;

/// [`MatchedConvertedConcreteId`] converter dedicated for `TrustBackedAssets`,
/// it is a similar implementation to `TrustBackedAssetsConvertedConcreteId`,
/// but it converts `AssetId` to `xcm::v*::Location` type instead of `AssetIdForTrustBackedAssets =
/// u32`
pub type TrustBackedAssetsAsLocation<
	TrustBackedAssetsPalletLocation,
	Balance,
	L,
	LocationConverter = WithLatestLocationConverter<L>,
> = MatchedConvertedConcreteId<
	L,
	Balance,
	StartsWith<TrustBackedAssetsPalletLocation>,
	LocationConverter,
	TryConvertInto,
>;

/// [`MatchedConvertedConcreteId`] converter dedicated for storing `ForeignAssets` with `AssetId` as
/// `Location`.
///
/// Excludes by default:
/// - parent as relay chain
/// - all local Locations
///
/// `AdditionalLocationExclusionFilter` can customize additional excluded Locations
pub type ForeignAssetsConvertedConcreteId<
	AdditionalLocationExclusionFilter,
	Balance,
	AssetId,
	LocationToAssetIdConverter = WithLatestLocationConverter<AssetId>,
	BalanceConverter = TryConvertInto,
> = MatchedConvertedConcreteId<
	AssetId,
	Balance,
	EverythingBut<(
		// Excludes relay/parent chain currency
		Equals<ParentLocation>,
		// Here we rely on fact that something like this works:
		// assert!(Location::new(1,
		// [Parachain(100)]).starts_with(&Location::parent()));
		// assert!([Parachain(100)].into().starts_with(&Here));
		StartsWith<LocalLocationPattern>,
		// Here we can exclude more stuff or leave it as `()`
		AdditionalLocationExclusionFilter,
	)>,
	LocationToAssetIdConverter,
	BalanceConverter,
>;

/// `Contains<Location>` implementation that matches locations with no parents,
/// a `PalletInstance` and an `AccountKey20` junction.
pub struct IsLocalAccountKey20;
impl Contains<Location> for IsLocalAccountKey20 {
	fn contains(location: &Location) -> bool {
		matches!(location.unpack(), (0, [AccountKey20 { .. }]))
	}
}

/// Fallible converter from a location to a `H160` that matches any location ending with
/// an `AccountKey20` junction.
pub struct AccountKey20ToH160;
impl MaybeEquivalence<Location, H160> for AccountKey20ToH160 {
	fn convert(location: &Location) -> Option<H160> {
		match location.unpack() {
			(0, [AccountKey20 { key, .. }]) => Some((*key).into()),
			_ => None,
		}
	}

	fn convert_back(key: &H160) -> Option<Location> {
		Some(Location::new(0, [AccountKey20 { key: (*key).into(), network: None }]))
	}
}

/// [`xcm_executor::traits::MatchesFungibles`] implementation that matches
/// ERC20 tokens.
pub type ERC20Matcher =
	MatchedConvertedConcreteId<H160, u128, IsLocalAccountKey20, AccountKey20ToH160, TryConvertInto>;

pub type AssetIdForPoolAssets = u32;

/// `Location` vs `AssetIdForPoolAssets` converter for `PoolAssets`.
pub type AssetIdForPoolAssetsConvert<PoolAssetsPalletLocation, L = Location> =
	AsPrefixedGeneralIndex<PoolAssetsPalletLocation, AssetIdForPoolAssets, TryConvertInto, L>;
/// [`MatchedConvertedConcreteId`] converter dedicated for `PoolAssets`
pub type PoolAssetsConvertedConcreteId<PoolAssetsPalletLocation, Balance> =
	MatchedConvertedConcreteId<
		AssetIdForPoolAssets,
		Balance,
		StartsWith<PoolAssetsPalletLocation>,
		AssetIdForPoolAssetsConvert<PoolAssetsPalletLocation>,
		TryConvertInto,
	>;

/// Adapter implementation for accessing pools (`pallet_asset_conversion`) that uses `AssetKind` as
/// a `xcm::v*` which could be different from the `xcm::latest`.
pub struct PoolAdapter<Runtime>(PhantomData<Runtime>);
impl<
		Runtime: pallet_asset_conversion::Config<PoolId = (L, L), AssetKind = L>,
		L: TryFrom<Location> + TryInto<Location> + Clone + Decode + EncodeLike + PartialEq,
	> PoolAdapter<Runtime>
{
	/// Returns a vector of all assets in a pool with `asset`.
	///
	/// Should only be used in runtime APIs since it iterates over the whole
	/// `pallet_asset_conversion::Pools` map.
	///
	/// It takes in any version of an XCM Location but always returns the latest one.
	/// This is to allow some margin of migrating the pools when updating the XCM version.
	///
	/// An error of type `()` is returned if the version conversion fails for XCM locations.
	/// This error should be mapped by the caller to a more descriptive one.
	pub fn get_assets_in_pool_with(asset: Location) -> Result<Vec<AssetId>, ()> {
		// convert latest to the `L` version.
		let asset: L = asset.try_into().map_err(|_| ())?;
		Self::iter_assets_in_pool_with(&asset)
			.map(|location| {
				// convert `L` to the latest `AssetId`
				location.try_into().map_err(|_| ()).map(AssetId)
			})
			.collect::<Result<Vec<_>, _>>()
	}

	/// Provides a current prices. Wrapper over
	/// `pallet_asset_conversion::Pallet::<T>::quote_price_tokens_for_exact_tokens`.
	///
	/// An error of type `()` is returned if the version conversion fails for XCM locations.
	/// This error should be mapped by the caller to a more descriptive one.
	pub fn quote_price_tokens_for_exact_tokens(
		asset_1: Location,
		asset_2: Location,
		amount: Runtime::Balance,
		include_fees: bool,
	) -> Result<Option<Runtime::Balance>, ()> {
		// Convert latest to the `L` version.
		let asset_1: L = asset_1.try_into().map_err(|_| ())?;
		let asset_2: L = asset_2.try_into().map_err(|_| ())?;

		// Quote swap price.
		Ok(pallet_asset_conversion::Pallet::<Runtime>::quote_price_tokens_for_exact_tokens(
			asset_1,
			asset_2,
			amount,
			include_fees,
		))
	}

	/// Helper function for filtering pool.
	pub fn iter_assets_in_pool_with(asset: &L) -> impl Iterator<Item = L> + '_ {
		pallet_asset_conversion::Pools::<Runtime>::iter_keys().filter_map(|(asset_1, asset_2)| {
			if asset_1 == *asset {
				Some(asset_2)
			} else if asset_2 == *asset {
				Some(asset_1)
			} else {
				None
			}
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_runtime::traits::MaybeEquivalence;
	use xcm_builder::{StartsWithExplicitGlobalConsensus, WithLatestLocationConverter};
	use xcm_executor::traits::{Error as MatchError, MatchesFungibles};

	#[test]
	fn asset_id_for_trust_backed_assets_convert_works() {
		frame_support::parameter_types! {
			pub TrustBackedAssetsPalletLocation: Location = Location::new(5, [PalletInstance(13)]);
		}
		let local_asset_id = 123456789 as AssetIdForTrustBackedAssets;
		let expected_reverse_ref =
			Location::new(5, [PalletInstance(13), GeneralIndex(local_asset_id.into())]);

		assert_eq!(
			AssetIdForTrustBackedAssetsConvert::<TrustBackedAssetsPalletLocation>::convert_back(
				&local_asset_id
			)
			.unwrap(),
			expected_reverse_ref
		);
		assert_eq!(
			AssetIdForTrustBackedAssetsConvert::<TrustBackedAssetsPalletLocation>::convert(
				&expected_reverse_ref
			)
			.unwrap(),
			local_asset_id
		);
	}

	#[test]
	fn trust_backed_assets_match_fungibles_works() {
		frame_support::parameter_types! {
			pub TrustBackedAssetsPalletLocation: Location = Location::new(0, [PalletInstance(13)]);
		}
		// set up a converter
		type TrustBackedAssetsConvert =
			TrustBackedAssetsConvertedConcreteId<TrustBackedAssetsPalletLocation, u128>;

		let test_data = vec![
			// missing GeneralIndex
			(ma_1000(0, [PalletInstance(13)].into()), Err(MatchError::AssetIdConversionFailed)),
			(
				ma_1000(0, [PalletInstance(13), GeneralKey { data: [0; 32], length: 32 }].into()),
				Err(MatchError::AssetIdConversionFailed),
			),
			(
				ma_1000(0, [PalletInstance(13), Parachain(1000)].into()),
				Err(MatchError::AssetIdConversionFailed),
			),
			// OK
			(ma_1000(0, [PalletInstance(13), GeneralIndex(1234)].into()), Ok((1234, 1000))),
			(
				ma_1000(0, [PalletInstance(13), GeneralIndex(1234), GeneralIndex(2222)].into()),
				Ok((1234, 1000)),
			),
			(
				ma_1000(
					0,
					[
						PalletInstance(13),
						GeneralIndex(1234),
						GeneralIndex(2222),
						GeneralKey { data: [0; 32], length: 32 },
					]
					.into(),
				),
				Ok((1234, 1000)),
			),
			// wrong pallet instance
			(
				ma_1000(0, [PalletInstance(77), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(0, [PalletInstance(77), GeneralIndex(1234), GeneralIndex(2222)].into()),
				Err(MatchError::AssetNotHandled),
			),
			// wrong parent
			(
				ma_1000(1, [PalletInstance(13), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(1, [PalletInstance(13), GeneralIndex(1234), GeneralIndex(2222)].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(1, [PalletInstance(77), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(1, [PalletInstance(77), GeneralIndex(1234), GeneralIndex(2222)].into()),
				Err(MatchError::AssetNotHandled),
			),
			// wrong parent
			(
				ma_1000(2, [PalletInstance(13), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(2, [PalletInstance(13), GeneralIndex(1234), GeneralIndex(2222)].into()),
				Err(MatchError::AssetNotHandled),
			),
			// missing GeneralIndex
			(ma_1000(0, [PalletInstance(77)].into()), Err(MatchError::AssetNotHandled)),
			(ma_1000(1, [PalletInstance(13)].into()), Err(MatchError::AssetNotHandled)),
			(ma_1000(2, [PalletInstance(13)].into()), Err(MatchError::AssetNotHandled)),
		];

		for (asset, expected_result) in test_data {
			assert_eq!(
				<TrustBackedAssetsConvert as MatchesFungibles<AssetIdForTrustBackedAssets, u128>>::matches_fungibles(&asset.clone().try_into().unwrap()),
				expected_result, "asset: {:?}", asset);
		}
	}

	#[test]
	fn foreign_assets_converted_concrete_id_converter_works() {
		frame_support::parameter_types! {
			pub Parachain100Pattern: Location = Location::new(1, [Parachain(100)]);
			pub UniversalLocationNetworkId: NetworkId = NetworkId::ByGenesis([9; 32]);
		}

		// set up a converter which uses `xcm::v4::Location` under the hood
		type Convert = ForeignAssetsConvertedConcreteId<
			(
				StartsWith<Parachain100Pattern>,
				StartsWithExplicitGlobalConsensus<UniversalLocationNetworkId>,
			),
			u128,
			xcm::v4::Location,
			WithLatestLocationConverter<xcm::v4::Location>,
		>;

		let test_data = vec![
			// excluded as local
			(ma_1000(0, Here), Err(MatchError::AssetNotHandled)),
			(ma_1000(0, [Parachain(100)].into()), Err(MatchError::AssetNotHandled)),
			(
				ma_1000(0, [PalletInstance(13), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			// excluded as parent
			(ma_1000(1, Here), Err(MatchError::AssetNotHandled)),
			// excluded as additional filter - Parachain100Pattern
			(ma_1000(1, [Parachain(100)].into()), Err(MatchError::AssetNotHandled)),
			(
				ma_1000(1, [Parachain(100), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(1, [Parachain(100), PalletInstance(13), GeneralIndex(1234)].into()),
				Err(MatchError::AssetNotHandled),
			),
			// excluded as additional filter - StartsWithExplicitGlobalConsensus
			(
				ma_1000(1, [GlobalConsensus(NetworkId::ByGenesis([9; 32]))].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(2, [GlobalConsensus(NetworkId::ByGenesis([9; 32]))].into()),
				Err(MatchError::AssetNotHandled),
			),
			(
				ma_1000(
					2,
					[
						GlobalConsensus(NetworkId::ByGenesis([9; 32])),
						Parachain(200),
						GeneralIndex(1234),
					]
					.into(),
				),
				Err(MatchError::AssetNotHandled),
			),
			// ok
			(
				ma_1000(1, [Parachain(200)].into()),
				Ok((xcm::v4::Location::new(1, [xcm::v4::Junction::Parachain(200)]), 1000)),
			),
			(
				ma_1000(2, [Parachain(200)].into()),
				Ok((xcm::v4::Location::new(2, [xcm::v4::Junction::Parachain(200)]), 1000)),
			),
			(
				ma_1000(1, [Parachain(200), GeneralIndex(1234)].into()),
				Ok((
					xcm::v4::Location::new(
						1,
						[xcm::v4::Junction::Parachain(200), xcm::v4::Junction::GeneralIndex(1234)],
					),
					1000,
				)),
			),
			(
				ma_1000(2, [Parachain(200), GeneralIndex(1234)].into()),
				Ok((
					xcm::v4::Location::new(
						2,
						[xcm::v4::Junction::Parachain(200), xcm::v4::Junction::GeneralIndex(1234)],
					),
					1000,
				)),
			),
			(
				ma_1000(2, [GlobalConsensus(NetworkId::ByGenesis([7; 32]))].into()),
				Ok((
					xcm::v4::Location::new(
						2,
						[xcm::v4::Junction::GlobalConsensus(xcm::v4::NetworkId::ByGenesis(
							[7; 32],
						))],
					),
					1000,
				)),
			),
			(
				ma_1000(
					2,
					[
						GlobalConsensus(NetworkId::ByGenesis([7; 32])),
						Parachain(200),
						GeneralIndex(1234),
					]
					.into(),
				),
				Ok((
					xcm::v4::Location::new(
						2,
						[
							xcm::v4::Junction::GlobalConsensus(xcm::v4::NetworkId::ByGenesis(
								[7; 32],
							)),
							xcm::v4::Junction::Parachain(200),
							xcm::v4::Junction::GeneralIndex(1234),
						],
					),
					1000,
				)),
			),
		];

		for (asset, expected_result) in test_data {
			assert_eq!(
				<Convert as MatchesFungibles<xcm::v4::Location, u128>>::matches_fungibles(
					&asset.clone().try_into().unwrap()
				),
				expected_result,
				"asset: {:?}",
				asset
			);
		}
	}

	// Create Asset
	fn ma_1000(parents: u8, interior: Junctions) -> Asset {
		(Location::new(parents, interior), 1000).into()
	}
}
