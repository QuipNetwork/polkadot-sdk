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

mod pallet_xcm_benchmarks_fungible;
mod pallet_xcm_benchmarks_generic;

use crate::{xcm_config::MaxAssetsIntoHolding, Runtime};
use alloc::vec::Vec;
use frame_support::weights::Weight;
use pallet_xcm_benchmarks_fungible::WeightInfo as XcmFungibleWeight;
use pallet_xcm_benchmarks_generic::WeightInfo as XcmGeneric;
use sp_runtime::BoundedVec;
use xcm::{
	latest::{prelude::*, AssetTransferFilter},
	DoubleEncoded,
};

trait WeighAssets {
	fn weigh_assets(&self, weight: Weight) -> Weight;
}

// Collectives only knows about WND.
const MAX_ASSETS: u64 = 1;

impl WeighAssets for AssetFilter {
	fn weigh_assets(&self, weight: Weight) -> Weight {
		match self {
			Self::Definite(assets) => weight.saturating_mul(assets.inner().iter().count() as u64),
			Self::Wild(asset) => match asset {
				All => weight.saturating_mul(MAX_ASSETS),
				AllOf { fun, .. } => match fun {
					WildFungibility::Fungible => weight,
					// Magic number 2 has to do with the fact that we could have up to 2 times
					// MaxAssetsIntoHolding in the worst-case scenario.
					WildFungibility::NonFungible => {
						weight.saturating_mul((MaxAssetsIntoHolding::get() * 2) as u64)
					},
				},
				AllCounted(count) => weight.saturating_mul(MAX_ASSETS.min(*count as u64)),
				AllOfCounted { count, .. } => weight.saturating_mul(MAX_ASSETS.min(*count as u64)),
			},
		}
	}
}

impl WeighAssets for Assets {
	fn weigh_assets(&self, weight: Weight) -> Weight {
		weight.saturating_mul(self.inner().iter().count() as u64)
	}
}

pub struct CollectivesWestendXcmWeight<Call>(core::marker::PhantomData<Call>);
impl<Call> XcmWeightInfo<Call> for CollectivesWestendXcmWeight<Call> {
	fn withdraw_asset(assets: &Assets) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::withdraw_asset())
	}
	fn reserve_asset_deposited(assets: &Assets) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::reserve_asset_deposited())
	}
	fn receive_teleported_asset(assets: &Assets) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::receive_teleported_asset())
	}
	fn query_response(
		_query_id: &u64,
		_response: &Response,
		_max_weight: &Weight,
		_querier: &Option<Location>,
	) -> Weight {
		XcmGeneric::<Runtime>::query_response()
	}
	fn transfer_asset(assets: &Assets, _dest: &Location) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::transfer_asset())
	}
	fn transfer_reserve_asset(assets: &Assets, _dest: &Location, _xcm: &Xcm<()>) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::transfer_reserve_asset())
	}
	fn transact(
		_origin_type: &OriginKind,
		_fallback_max_weight: &Option<Weight>,
		_call: &DoubleEncoded<Call>,
	) -> Weight {
		XcmGeneric::<Runtime>::transact()
	}
	fn hrmp_new_channel_open_request(
		_sender: &u32,
		_max_message_size: &u32,
		_max_capacity: &u32,
	) -> Weight {
		// XCM Executor does not currently support HRMP channel operations
		Weight::MAX
	}
	fn hrmp_channel_accepted(_recipient: &u32) -> Weight {
		// XCM Executor does not currently support HRMP channel operations
		Weight::MAX
	}
	fn hrmp_channel_closing(_initiator: &u32, _sender: &u32, _recipient: &u32) -> Weight {
		// XCM Executor does not currently support HRMP channel operations
		Weight::MAX
	}
	fn clear_origin() -> Weight {
		XcmGeneric::<Runtime>::clear_origin()
	}
	fn descend_origin(_who: &InteriorLocation) -> Weight {
		XcmGeneric::<Runtime>::descend_origin()
	}
	fn report_error(_query_response_info: &QueryResponseInfo) -> Weight {
		XcmGeneric::<Runtime>::report_error()
	}

	fn deposit_asset(assets: &AssetFilter, _dest: &Location) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::deposit_asset())
	}
	fn deposit_reserve_asset(assets: &AssetFilter, _dest: &Location, _xcm: &Xcm<()>) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::deposit_reserve_asset())
	}
	fn exchange_asset(_give: &AssetFilter, _receive: &Assets, _maximal: &bool) -> Weight {
		Weight::MAX
	}
	fn initiate_reserve_withdraw(
		assets: &AssetFilter,
		_reserve: &Location,
		_xcm: &Xcm<()>,
	) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::initiate_reserve_withdraw())
	}
	fn initiate_teleport(assets: &AssetFilter, _dest: &Location, _xcm: &Xcm<()>) -> Weight {
		assets.weigh_assets(XcmFungibleWeight::<Runtime>::initiate_teleport())
	}
	fn initiate_transfer(
		_dest: &Location,
		remote_fees: &Option<AssetTransferFilter>,
		_preserve_origin: &bool,
		assets: &BoundedVec<AssetTransferFilter, MaxAssetTransferFilters>,
		_xcm: &Xcm<()>,
	) -> Weight {
		let mut weight = if let Some(remote_fees) = remote_fees {
			let fees = remote_fees.inner();
			fees.weigh_assets(XcmFungibleWeight::<Runtime>::initiate_transfer())
		} else {
			Weight::zero()
		};
		for asset_filter in assets {
			let assets = asset_filter.inner();
			let extra = assets.weigh_assets(XcmFungibleWeight::<Runtime>::initiate_transfer());
			weight = weight.saturating_add(extra);
		}
		weight
	}
	fn report_holding(_response_info: &QueryResponseInfo, _assets: &AssetFilter) -> Weight {
		XcmGeneric::<Runtime>::report_holding()
	}
	fn buy_execution(_fees: &Asset, _weight_limit: &WeightLimit) -> Weight {
		XcmGeneric::<Runtime>::buy_execution()
	}
	fn pay_fees(_asset: &Asset) -> Weight {
		XcmGeneric::<Runtime>::pay_fees()
	}
	fn refund_surplus() -> Weight {
		XcmGeneric::<Runtime>::refund_surplus()
	}
	fn set_error_handler(_xcm: &Xcm<Call>) -> Weight {
		XcmGeneric::<Runtime>::set_error_handler()
	}
	fn set_appendix(_xcm: &Xcm<Call>) -> Weight {
		XcmGeneric::<Runtime>::set_appendix()
	}
	fn clear_error() -> Weight {
		XcmGeneric::<Runtime>::clear_error()
	}
	fn set_hints(hints: &BoundedVec<Hint, HintNumVariants>) -> Weight {
		let mut weight = Weight::zero();
		for hint in hints {
			match hint {
				AssetClaimer { .. } => {
					weight = weight.saturating_add(XcmGeneric::<Runtime>::asset_claimer());
				},
			}
		}
		weight
	}
	fn claim_asset(assets: &Assets, _ticket: &Location) -> Weight {
		assets.weigh_assets(XcmGeneric::<Runtime>::claim_asset())
	}
	fn trap(_code: &u64) -> Weight {
		XcmGeneric::<Runtime>::trap()
	}
	fn subscribe_version(_query_id: &QueryId, _max_response_weight: &Weight) -> Weight {
		XcmGeneric::<Runtime>::subscribe_version()
	}
	fn unsubscribe_version() -> Weight {
		XcmGeneric::<Runtime>::unsubscribe_version()
	}
	fn burn_asset(assets: &Assets) -> Weight {
		assets.weigh_assets(XcmGeneric::<Runtime>::burn_asset())
	}
	fn expect_asset(assets: &Assets) -> Weight {
		assets.weigh_assets(XcmGeneric::<Runtime>::expect_asset())
	}
	fn expect_origin(_origin: &Option<Location>) -> Weight {
		XcmGeneric::<Runtime>::expect_origin()
	}
	fn expect_error(_error: &Option<(u32, XcmError)>) -> Weight {
		XcmGeneric::<Runtime>::expect_error()
	}
	fn expect_transact_status(_transact_status: &MaybeErrorCode) -> Weight {
		XcmGeneric::<Runtime>::expect_transact_status()
	}
	fn query_pallet(_module_name: &Vec<u8>, _response_info: &QueryResponseInfo) -> Weight {
		XcmGeneric::<Runtime>::query_pallet()
	}
	fn expect_pallet(
		_index: &u32,
		_name: &Vec<u8>,
		_module_name: &Vec<u8>,
		_crate_major: &u32,
		_min_crate_minor: &u32,
	) -> Weight {
		XcmGeneric::<Runtime>::expect_pallet()
	}
	fn report_transact_status(_response_info: &QueryResponseInfo) -> Weight {
		XcmGeneric::<Runtime>::report_transact_status()
	}
	fn clear_transact_status() -> Weight {
		XcmGeneric::<Runtime>::clear_transact_status()
	}
	fn universal_origin(_: &Junction) -> Weight {
		Weight::MAX
	}
	fn export_message(_: &NetworkId, _: &Junctions, _: &Xcm<()>) -> Weight {
		Weight::MAX
	}
	fn lock_asset(_: &Asset, _: &Location) -> Weight {
		Weight::MAX
	}
	fn unlock_asset(_: &Asset, _: &Location) -> Weight {
		Weight::MAX
	}
	fn note_unlockable(_: &Asset, _: &Location) -> Weight {
		Weight::MAX
	}
	fn request_unlock(_: &Asset, _: &Location) -> Weight {
		Weight::MAX
	}
	fn set_fees_mode(_: &bool) -> Weight {
		XcmGeneric::<Runtime>::set_fees_mode()
	}
	fn set_topic(_topic: &[u8; 32]) -> Weight {
		XcmGeneric::<Runtime>::set_topic()
	}
	fn clear_topic() -> Weight {
		XcmGeneric::<Runtime>::clear_topic()
	}
	fn alias_origin(_: &Location) -> Weight {
		XcmGeneric::<Runtime>::alias_origin()
	}
	fn unpaid_execution(_: &WeightLimit, _: &Option<Location>) -> Weight {
		XcmGeneric::<Runtime>::unpaid_execution()
	}
	fn execute_with_origin(_: &Option<InteriorLocation>, _: &Xcm<Call>) -> Weight {
		XcmGeneric::<Runtime>::execute_with_origin()
	}
}
