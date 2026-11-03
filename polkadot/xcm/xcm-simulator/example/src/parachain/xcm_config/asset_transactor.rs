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

use crate::parachain::{
	constants::KsmLocation, location_converter::LocationConverter, AccountId, Balances,
	ForeignUniques,
};
use sp_runtime::traits::TryConvertInto;
use xcm::latest::prelude::*;
use xcm_builder::{
	ConvertedConcreteId, FungibleAdapter, IsConcrete, NoChecking, NonFungiblesAdapter,
};

type LocalAssetTransactor = (
	FungibleAdapter<Balances, IsConcrete<KsmLocation>, LocationConverter, AccountId, ()>,
	NonFungiblesAdapter<
		ForeignUniques,
		ConvertedConcreteId<Location, AssetInstance, TryConvertInto, TryConvertInto>,
		LocationConverter,
		AccountId,
		NoChecking,
		(),
	>,
);

pub type AssetTransactor = LocalAssetTransactor;
