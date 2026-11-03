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

pub use penpal_runtime::{
	self, xcm_config::RelayNetworkId as PenpalRelayNetworkId, ForeignAssetReserveData,
};

mod genesis;
pub use genesis::{genesis, PenpalAssetOwner, PenpalSudoAccount, ED, PARA_ID_A, PARA_ID_B};

// Substrate
use frame_support::traits::OnInitialize;
use sp_core::Encode;

// Cumulus
use emulated_integration_tests_common::{
	impl_accounts_helpers_for_parachain, impl_assert_events_helpers_for_parachain,
	impl_foreign_assets_helpers_for_parachain, impl_xcm_helpers_for_parachain,
	impls::{NetworkId, Parachain},
	xcm_emulator::decl_test_parachains,
};

// Polkadot
use xcm::latest::{ROCOCO_GENESIS_HASH, WESTEND_GENESIS_HASH};

// Penpal Parachain declaration
decl_test_parachains! {
	pub struct PenpalA {
		genesis = genesis(PARA_ID_A),
		on_init = {
			penpal_runtime::AuraExt::on_initialize(1);
			frame_support::assert_ok!(penpal_runtime::System::set_storage(
				penpal_runtime::RuntimeOrigin::root(),
				vec![(PenpalRelayNetworkId::key().to_vec(), NetworkId::ByGenesis(ROCOCO_GENESIS_HASH).encode())],
			));
		},
		runtime = penpal_runtime,
		core = {
			XcmpMessageHandler: penpal_runtime::XcmpQueue,
			LocationToAccountId: penpal_runtime::xcm_config::LocationToAccountId,
			ParachainInfo: penpal_runtime::ParachainInfo,
			MessageOrigin: cumulus_primitives_core::AggregateMessageOrigin,
		},
		pallets = {
			PolkadotXcm: penpal_runtime::PolkadotXcm,
			Assets: penpal_runtime::Assets,
			AssetConversion: penpal_runtime::AssetConversion,
			Balances: penpal_runtime::Balances,
		}
	},
	pub struct PenpalB {
		genesis = genesis(PARA_ID_B),
		on_init = {
			penpal_runtime::AuraExt::on_initialize(1);
			frame_support::assert_ok!(penpal_runtime::System::set_storage(
				penpal_runtime::RuntimeOrigin::root(),
				vec![(PenpalRelayNetworkId::key().to_vec(), NetworkId::ByGenesis(WESTEND_GENESIS_HASH).encode())],
			));
		},
		runtime = penpal_runtime,
		core = {
			XcmpMessageHandler: penpal_runtime::XcmpQueue,
			LocationToAccountId: penpal_runtime::xcm_config::LocationToAccountId,
			ParachainInfo: penpal_runtime::ParachainInfo,
			MessageOrigin: cumulus_primitives_core::AggregateMessageOrigin,
		},
		pallets = {
			PolkadotXcm: penpal_runtime::PolkadotXcm,
			Assets: penpal_runtime::Assets,
			AssetConversion: penpal_runtime::AssetConversion,
			Balances: penpal_runtime::Balances,
		}
	},
}

// Penpal implementation
impl_accounts_helpers_for_parachain!(PenpalA);
impl_accounts_helpers_for_parachain!(PenpalB);
impl_assert_events_helpers_for_parachain!(PenpalA);
impl_assert_events_helpers_for_parachain!(PenpalB);
impl_foreign_assets_helpers_for_parachain!(
	PenpalA,
	xcm::latest::Location,
	ForeignAssetReserveData,
	Assets
);
impl_foreign_assets_helpers_for_parachain!(
	PenpalB,
	xcm::latest::Location,
	ForeignAssetReserveData,
	Assets
);
impl_xcm_helpers_for_parachain!(PenpalA);
impl_xcm_helpers_for_parachain!(PenpalB);
