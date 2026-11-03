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

pub use pallet::*;

#[cfg(feature = "runtime-benchmarks")]
pub mod benchmarking;
#[cfg(test)]
mod mock;

#[frame_support::pallet]
pub mod pallet {
	use frame_benchmarking::BenchmarkError;
	use frame_support::{dispatch::GetDispatchInfo, pallet_prelude::Encode};
	use sp_runtime::traits::Dispatchable;
	use xcm::latest::{
		Asset, Assets, InteriorLocation, Junction, Location, NetworkId, Response, WeightLimit,
	};

	#[pallet::config]
	pub trait Config<I: 'static = ()>: frame_system::Config + crate::Config {
		type RuntimeCall: Dispatchable<RuntimeOrigin = Self::RuntimeOrigin>
			+ GetDispatchInfo
			+ From<frame_system::Call<Self>>
			+ Encode;

		/// The type of `fungible` that is being used under the hood.
		///
		/// This is useful for testing and checking.
		type TransactAsset: frame_support::traits::fungible::Mutate<Self::AccountId>;

		/// 	The response which causes the most runtime weight.
		fn worst_case_response() -> (u64, Response);

		/// The pair of asset collections which causes the most runtime weight if demanded to be
		/// exchanged.
		///
		/// The first element in the returned tuple represents the assets that are being exchanged
		/// from, whereas the second element represents the assets that are being exchanged to.
		///
		/// If set to `Err`, benchmarks which rely on an `exchange_asset` will be skipped.
		fn worst_case_asset_exchange() -> Result<(Assets, Assets), BenchmarkError>;

		/// A `(Location, Junction)` that is one of the `UniversalAliases` configured by the
		/// XCM executor.
		///
		/// If set to `Err`, benchmarks which rely on a universal alias will be skipped.
		fn universal_alias() -> Result<(Location, Junction), BenchmarkError>;

		/// The `Location` and `RuntimeCall` used for successful transaction XCMs.
		///
		/// If set to `Err`, benchmarks which rely on a `transact_origin_and_runtime_call` will be
		/// skipped.
		fn transact_origin_and_runtime_call(
		) -> Result<(Location, <Self as crate::generic::Config<I>>::RuntimeCall), BenchmarkError>;

		/// A valid `Location` we can successfully subscribe to.
		///
		/// If set to `Err`, benchmarks which rely on a `subscribe_origin` will be skipped.
		fn subscribe_origin() -> Result<Location, BenchmarkError>;

		/// Return an origin, ticket, and assets that can be trapped and claimed.
		fn claimable_asset() -> Result<(Location, Location, Assets), BenchmarkError>;

		/// The worst case buy execution weight limit and
		/// asset to trigger the Trader::buy_execution in the XCM executor
		/// Used to buy weight in benchmarks, for example in
		/// `refund_surplus`.
		fn worst_case_for_trader() -> Result<(Asset, WeightLimit), BenchmarkError>;

		/// Return an unlocker, owner and assets that can be locked and unlocked.
		fn unlockable_asset() -> Result<(Location, Location, Asset), BenchmarkError>;

		/// A `(Location, NetworkId, InteriorLocation)` we can successfully export message
		/// to.
		///
		/// If set to `Err`, benchmarks which rely on `export_message` will be skipped.
		fn export_message_origin_and_destination(
		) -> Result<(Location, NetworkId, InteriorLocation), BenchmarkError>;

		/// A `(Location, Location)` that is matched by one of the `Aliasers` configured by the XCM
		/// executor.
		///
		/// Should be the *most expensive* such pair, since `Aliasers` is tried in order and stops
		/// at the first match: a pair matched by the first, storage-free filter measures none of
		/// the work a later one does. Chains whose last aliaser consults an on-chain
		/// authorization list must also set that list up at its maximum size.
		///
		/// If set to `Err`, benchmarks which rely on a universal alias will be skipped.
		fn alias_origin() -> Result<(Location, Location), BenchmarkError>;

		/// Returns a valid pallet info for `ExpectPallet` or `QueryPallet` benchmark.
		///
		/// By default returns `frame_system::Pallet` info with expected pallet index `0`.
		fn valid_pallet() -> frame_support::traits::PalletInfoData {
			frame_support::traits::PalletInfoData {
				index: <frame_system::Pallet<Self> as frame_support::traits::PalletInfoAccess>::index(),
				name: <frame_system::Pallet<Self> as frame_support::traits::PalletInfoAccess>::name(),
				module_name: <frame_system::Pallet<Self> as frame_support::traits::PalletInfoAccess>::module_name(),
				crate_version: <frame_system::Pallet<Self> as frame_support::traits::PalletInfoAccess>::crate_version(),
			}
		}
	}

	#[pallet::pallet]
	pub struct Pallet<T, I = ()>(_);
}
