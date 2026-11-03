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

#[cfg(test)]
pub use emulated_integration_tests_common::{
	asset_exists_on, assets_balance_on, assets_issuance_on, balances_issuance_on,
	foreign_balance_on, foreign_issuance_on,
};

#[cfg(test)]
mod imports {
	pub(crate) use codec::Encode;

	// Substrate
	pub(crate) use frame_support::{
		assert_err, assert_ok,
		pallet_prelude::Weight,
		sp_runtime::{DispatchError, DispatchResult, ModuleError},
		traits::fungibles::Inspect,
		BoundedVec,
	};

	// Polkadot
	pub(crate) use xcm::{
		latest::{AssetTransferFilter, ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH},
		prelude::{AccountId32 as AccountId32Junction, *},
	};
	pub(crate) use xcm_executor::traits::TransferType;

	// Cumulus
	pub(crate) use asset_test_utils::xcm_helpers;
	pub(crate) use emulated_integration_tests_common::{
		accounts::DUMMY_EMPTY,
		create_foreign_pool_with_native_on, create_foreign_pool_with_parent_native_on,
		create_pool_with_relay_native_on, local_penpal_asset, test_can_estimate_and_pay_exact_fees,
		test_parachain_is_trusted_teleporter, test_parachain_is_trusted_teleporter_for_relay,
		test_relay_is_trusted_teleporter, test_xcm_fee_querying_apis_work_for_asset_hub,
		xcm_emulator::{
			assert_expected_events, bx, Chain, Parachain as Para, RelayChain as Relay, Test,
			TestArgs, TestContext, TestExt,
		},
		xcm_helpers::{
			fee_asset, find_mq_processed_id, find_xcm_sent_message_id,
			get_amount_from_versioned_assets, non_fee_asset, xcm_transact_paid_execution,
		},
		xcm_simulator::helpers::TopicIdTracker,
		PenpalALocation, PenpalAPen2TeleportableAssetLocation,
		PenpalBPen2TeleportableAssetLocation, ASSETS_PALLET_ID, RESERVABLE_ASSET_ID, USDT_ID,
		XCM_V3,
	};
	pub(crate) use parachains_common::{AccountId, Balance};
	pub(crate) use westend_system_emulated_network::{
		asset_hub_westend_emulated_chain::{
			asset_hub_westend_runtime::{
				self,
				xcm_config::{
					self as ahw_xcm_config, DapBufferAccount, WestendLocation as RelayLocation,
					XcmConfig as AssetHubWestendXcmConfig,
				},
				AssetConversionOrigin as AssetHubWestendAssetConversionOrigin,
				ExistentialDeposit as AssetHubWestendExistentialDeposit, ForeignAssetReserveData,
			},
			genesis::{AssetHubWestendAssetOwner, ED as ASSET_HUB_WESTEND_ED},
			AssetHubWestendParaPallet as AssetHubWestendPallet,
		},
		bridge_hub_westend_emulated_chain::{
			bridge_hub_westend_runtime::xcm_config::{self as bhw_xcm_config},
			BridgeHubWestendParaPallet as BridgeHubWestendPallet,
		},
		collectives_westend_emulated_chain::CollectivesWestendParaPallet as CollectivesWestendPallet,
		coretime_westend_emulated_chain::CoretimeWestendParaPallet as CoretimeWestendPallet,
		penpal_emulated_chain::{
			penpal_runtime::xcm_config::{
				CustomizableAssetFromSystemAssetHub as PenpalCustomizableAssetFromSystemAssetHub,
				LocalPen2Asset as PenpalLocalPen2Asset,
				LocalReservableFromAssetHub as PenpalLocalReservableFromAssetHub,
				LocalTeleportableToAssetHub as PenpalLocalTeleportableToAssetHub,
				UniversalLocation as PenpalUniversalLocation,
				UsdtFromAssetHub as PenpalUsdtFromAssetHub,
			},
			PenpalAParaPallet as PenpalAPallet, PenpalAssetOwner,
			PenpalBParaPallet as PenpalBPallet,
		},
		people_westend_emulated_chain::PeopleWestendParaPallet as PeopleWestendPallet,
		westend_emulated_chain::{
			genesis::ED as WESTEND_ED,
			westend_runtime::{
				xcm_config::{
					UniversalLocation as WestendUniversalLocation, XcmConfig as WestendXcmConfig,
				},
				Dmp,
			},
			WestendRelayPallet as WestendPallet,
		},
		AssetHubWestendPara as AssetHubWestend,
		AssetHubWestendParaReceiver as AssetHubWestendReceiver,
		AssetHubWestendParaSender as AssetHubWestendSender,
		BridgeHubWestendPara as BridgeHubWestend,
		BridgeHubWestendParaReceiver as BridgeHubWestendReceiver,
		CollectivesWestendPara as CollectivesWestend, CoretimeWestendPara as CoretimeWestend,
		PenpalAPara as PenpalA, PenpalAParaReceiver as PenpalAReceiver,
		PenpalAParaSender as PenpalASender, PenpalBPara as PenpalB,
		PenpalBParaReceiver as PenpalBReceiver, PeopleWestendPara as PeopleWestend,
		WestendRelay as Westend, WestendRelayReceiver as WestendReceiver,
		WestendRelaySender as WestendSender,
	};

	pub(crate) const ASSET_ID: u32 = 3;
	pub(crate) const ASSET_MIN_BALANCE: u128 = 1000;

	pub(crate) type RelayToParaTest = Test<Westend, PenpalA>;
	pub(crate) type ParaToRelayTest = Test<PenpalA, Westend>;
	pub(crate) type RelayToSystemParaTest = Test<Westend, AssetHubWestend>;
	pub(crate) type SystemParaToRelayTest = Test<AssetHubWestend, Westend>;
	pub(crate) type SystemParaToParaTest = Test<AssetHubWestend, PenpalA, (), TestArgs<Location>>;
	pub(crate) type ParaToSystemParaTest = Test<PenpalA, AssetHubWestend, (), TestArgs<Location>>;
	pub(crate) type ParaToParaThroughRelayTest =
		Test<PenpalA, PenpalB, Westend, TestArgs<Location>>;
	pub(crate) type ParaToParaThroughAHTest =
		Test<PenpalA, PenpalB, AssetHubWestend, TestArgs<Location>>;
	pub(crate) type RelayToParaThroughAHTest = Test<Westend, PenpalA, AssetHubWestend>;
	pub(crate) type PenpalToRelayThroughAHTest = Test<PenpalA, Westend, AssetHubWestend>;
}

#[cfg(test)]
mod tests;
