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

//! Trait for providing methods to mutate liquidity pools.

use frame_support::{traits::tokens::Balance, transactional};
use sp_runtime::DispatchError;

use crate::{Config, Pallet};

/// A struct to represent an asset and its desired and minimum amounts for adding liquidity.
pub struct AddLiquidityAsset<AssetKind, Balance> {
	/// The kind of asset.
	pub asset: AssetKind,
	/// The desired amount of the asset to add.
	pub amount_desired: Balance,
	/// The minimum amount of the asset to add.
	pub amount_min: Balance,
}

/// Trait for providing methods to mutate liquidity pools. This includes creating pools,
/// adding liquidity, and removing liquidity.
pub trait MutateLiquidity<AccountId> {
	/// The balance type for assets.
	type Balance: Balance;
	/// The type used to identify assets.
	type AssetKind;
	/// The type used to identify a liquidity pool.
	type PoolId;

	/// Creates a new liquidity pool for the given assets.
	///
	/// Mints LP tokens to the `creator` account.
	///
	/// Returns the ID of the newly created pool.
	fn create_pool(
		creator: &AccountId,
		asset1: Self::AssetKind,
		asset2: Self::AssetKind,
	) -> Result<Self::PoolId, DispatchError>;

	/// Adds liquidity to an existing pool.
	///
	/// Mints LP tokens to the `mint_to` account.
	///
	/// Returns the amount of LP tokens minted.
	fn add_liquidity(
		who: &AccountId,
		asset1: AddLiquidityAsset<Self::AssetKind, Self::Balance>,
		asset2: AddLiquidityAsset<Self::AssetKind, Self::Balance>,
		mint_to: &AccountId,
	) -> Result<Self::Balance, DispatchError>;

	/// Removes liquidity from a pool.
	///
	/// Burns LP tokens from the `who` account and transfers the withdrawn assets to the
	/// `withdraw_to` account.
	///
	/// Returns the amounts of assets withdrawn.
	fn remove_liquidity(
		who: &AccountId,
		asset1: Self::AssetKind,
		asset2: Self::AssetKind,
		lp_token_burn: Self::Balance,
		amount1_min_receive: Self::Balance,
		amount2_min_receive: Self::Balance,
		withdraw_to: &AccountId,
	) -> Result<(Self::Balance, Self::Balance), DispatchError>;
}

impl<T: Config> MutateLiquidity<T::AccountId> for Pallet<T> {
	type Balance = T::Balance;
	type AssetKind = T::AssetKind;
	type PoolId = T::PoolId;

	#[transactional]
	fn create_pool(
		creator: &T::AccountId,
		asset1: T::AssetKind,
		asset2: T::AssetKind,
	) -> Result<T::PoolId, DispatchError> {
		Self::do_create_pool(creator, asset1, asset2, None)
	}

	#[transactional]
	fn add_liquidity(
		who: &T::AccountId,
		asset1: AddLiquidityAsset<Self::AssetKind, Self::Balance>,
		asset2: AddLiquidityAsset<Self::AssetKind, Self::Balance>,
		mint_to: &T::AccountId,
	) -> Result<T::Balance, DispatchError> {
		Self::do_add_liquidity(
			who,
			asset1.asset,
			asset2.asset,
			asset1.amount_desired,
			asset2.amount_desired,
			asset1.amount_min,
			asset2.amount_min,
			mint_to,
		)
	}

	#[transactional]
	fn remove_liquidity(
		who: &T::AccountId,
		asset1: T::AssetKind,
		asset2: T::AssetKind,
		lp_token_burn: T::Balance,
		amount1_min_receive: T::Balance,
		amount2_min_receive: T::Balance,
		withdraw_to: &T::AccountId,
	) -> Result<(T::Balance, T::Balance), DispatchError> {
		Self::do_remove_liquidity(
			who,
			asset1,
			asset2,
			lp_token_burn,
			amount1_min_receive,
			amount2_min_receive,
			withdraw_to,
		)
	}
}
