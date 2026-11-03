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

use crate::relay_chain::{
	constants::TokenLocation, location_converter::LocationConverter, AccountId, Balances, Uniques,
};
use sp_runtime::traits::TryConvertInto;
use xcm_builder::{
	AsPrefixedGeneralIndex, ConvertedConcreteId, FungibleAdapter, IsConcrete, NoChecking,
	NonFungiblesAdapter,
};

type LocalAssetTransactor = (
	FungibleAdapter<Balances, IsConcrete<TokenLocation>, LocationConverter, AccountId, ()>,
	NonFungiblesAdapter<
		Uniques,
		ConvertedConcreteId<
			u32,
			u32,
			AsPrefixedGeneralIndex<(), u32, TryConvertInto>,
			TryConvertInto,
		>,
		LocationConverter,
		AccountId,
		NoChecking,
		(),
	>,
);

pub type AssetTransactor = LocalAssetTransactor;
