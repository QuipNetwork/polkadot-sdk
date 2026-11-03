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

//! Runtime API definition for assets.

use crate::runtime_api::FungiblesAccessError;
use alloc::vec::Vec;
use core::borrow::Borrow;
use frame_support::traits::Contains;
use sp_runtime::traits::MaybeEquivalence;
use xcm::latest::{Asset, Location};
use xcm_builder::{ConvertedConcreteId, MatchedConvertedConcreteId};
use xcm_executor::traits::MatchesFungibles;

/// Converting any [`(AssetId, Balance)`] to [`Asset`]
pub trait AssetConverter<AssetId, Balance, ConvertAssetId, ConvertBalance>:
	MatchesFungibles<AssetId, Balance>
where
	AssetId: Clone,
	Balance: Clone,
	ConvertAssetId: MaybeEquivalence<Location, AssetId>,
	ConvertBalance: MaybeEquivalence<u128, Balance>,
{
	fn convert_ref(value: impl Borrow<(AssetId, Balance)>) -> Result<Asset, FungiblesAccessError>;
}

/// Checks for `Location`.
pub trait MatchesLocation<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertBalance>:
	MatchesFungibles<AssetId, Balance>
where
	AssetId: Clone,
	Balance: Clone,
	MatchAssetId: Contains<Location>,
	ConvertAssetId: MaybeEquivalence<Location, AssetId>,
	ConvertBalance: MaybeEquivalence<u128, Balance>,
{
	fn contains(location: &Location) -> bool;
}

impl<
		AssetId: Clone,
		Balance: Clone,
		ConvertAssetId: MaybeEquivalence<Location, AssetId>,
		ConvertBalance: MaybeEquivalence<u128, Balance>,
	> AssetConverter<AssetId, Balance, ConvertAssetId, ConvertBalance>
	for ConvertedConcreteId<AssetId, Balance, ConvertAssetId, ConvertBalance>
{
	fn convert_ref(value: impl Borrow<(AssetId, Balance)>) -> Result<Asset, FungiblesAccessError> {
		let (asset_id, balance) = value.borrow();
		match ConvertAssetId::convert_back(asset_id) {
			Some(asset_id_as_location) => match ConvertBalance::convert_back(balance) {
				Some(amount) => Ok((asset_id_as_location, amount).into()),
				None => Err(FungiblesAccessError::AmountToBalanceConversionFailed),
			},
			None => Err(FungiblesAccessError::AssetIdConversionFailed),
		}
	}
}

impl<
		AssetId: Clone,
		Balance: Clone,
		MatchAssetId: Contains<Location>,
		ConvertAssetId: MaybeEquivalence<Location, AssetId>,
		ConvertBalance: MaybeEquivalence<u128, Balance>,
	> AssetConverter<AssetId, Balance, ConvertAssetId, ConvertBalance>
	for MatchedConvertedConcreteId<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertBalance>
{
	fn convert_ref(value: impl Borrow<(AssetId, Balance)>) -> Result<Asset, FungiblesAccessError> {
		let (asset_id, balance) = value.borrow();
		match ConvertAssetId::convert_back(asset_id) {
			Some(asset_id_as_location) => match ConvertBalance::convert_back(balance) {
				Some(amount) => Ok((asset_id_as_location, amount).into()),
				None => Err(FungiblesAccessError::AmountToBalanceConversionFailed),
			},
			None => Err(FungiblesAccessError::AssetIdConversionFailed),
		}
	}
}

impl<
		AssetId: Clone,
		Balance: Clone,
		MatchAssetId: Contains<Location>,
		ConvertAssetId: MaybeEquivalence<Location, AssetId>,
		ConvertBalance: MaybeEquivalence<u128, Balance>,
	> MatchesLocation<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertBalance>
	for MatchedConvertedConcreteId<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertBalance>
{
	fn contains(location: &Location) -> bool {
		MatchAssetId::contains(location)
	}
}

#[impl_trait_for_tuples::impl_for_tuples(30)]
impl<
		AssetId: Clone,
		Balance: Clone,
		MatchAssetId: Contains<Location>,
		ConvertAssetId: MaybeEquivalence<Location, AssetId>,
		ConvertBalance: MaybeEquivalence<u128, Balance>,
	> MatchesLocation<AssetId, Balance, MatchAssetId, ConvertAssetId, ConvertBalance> for Tuple
{
	fn contains(location: &Location) -> bool {
		for_tuples!( #(
			match Tuple::contains(location) { o @ true => return o, _ => () }
		)* );
		tracing::trace!(target: "xcm::contains", ?location, "MatchesLocation: no match");
		false
	}
}

/// Helper function to convert collections with [`(AssetId, Balance)`] to [`Asset`]
pub fn convert<'a, AssetId, Balance, ConvertAssetId, ConvertBalance, Converter>(
	items: impl Iterator<Item = &'a (AssetId, Balance)>,
) -> Result<Vec<Asset>, FungiblesAccessError>
where
	AssetId: Clone + 'a,
	Balance: Clone + 'a,
	ConvertAssetId: MaybeEquivalence<Location, AssetId>,
	ConvertBalance: MaybeEquivalence<u128, Balance>,
	Converter: AssetConverter<AssetId, Balance, ConvertAssetId, ConvertBalance>,
{
	items.map(Converter::convert_ref).collect()
}

/// Helper function to convert `Balance` with Location` to `Asset`
pub fn convert_balance<T: frame_support::pallet_prelude::Get<Location>, Balance: TryInto<u128>>(
	balance: Balance,
) -> Result<Asset, FungiblesAccessError> {
	match balance.try_into() {
		Ok(balance) => Ok((T::get(), balance).into()),
		Err(_) => Err(FungiblesAccessError::AmountToBalanceConversionFailed),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use frame_support::traits::Everything;

	use sp_runtime::traits::{Identity, TryConvertInto};
	use xcm::latest::prelude::*;

	type Converter =
		MatchedConvertedConcreteId<Location, u64, Everything, Identity, TryConvertInto>;

	#[test]
	fn converted_concrete_id_fungible_multi_asset_conversion_roundtrip_works() {
		let location = Location::new(0, [GlobalConsensus(ByGenesis([0; 32]))]);
		let amount = 123456_u64;
		let expected_multi_asset = Asset {
			id: AssetId(Location::new(0, [GlobalConsensus(ByGenesis([0; 32]))])),
			fun: Fungible(123456_u128),
		};

		assert_eq!(
			Converter::matches_fungibles(&expected_multi_asset).map_err(|_| ()),
			Ok((location.clone(), amount))
		);

		assert_eq!(Converter::convert_ref((location, amount)), Ok(expected_multi_asset));
	}

	#[test]
	fn converted_concrete_id_fungible_multi_asset_conversion_collection_works() {
		let data = vec![
			(Location::new(0, [GlobalConsensus(ByGenesis([0; 32]))]), 123456_u64),
			(Location::new(1, [GlobalConsensus(ByGenesis([1; 32]))]), 654321_u64),
		];

		let expected_data = vec![
			Asset {
				id: AssetId(Location::new(0, [GlobalConsensus(ByGenesis([0; 32]))])),
				fun: Fungible(123456_u128),
			},
			Asset {
				id: AssetId(Location::new(1, [GlobalConsensus(ByGenesis([1; 32]))])),
				fun: Fungible(654321_u128),
			},
		];

		assert_eq!(convert::<_, _, _, _, Converter>(data.iter()), Ok(expected_data));
	}
}
