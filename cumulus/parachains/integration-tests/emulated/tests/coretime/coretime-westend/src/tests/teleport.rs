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

use crate::imports::*;
use emulated_integration_tests_common::{
	test_parachain_is_trusted_teleporter, test_parachain_is_trusted_teleporter_for_relay,
	test_relay_is_trusted_teleporter,
};

#[test]
fn teleport_via_limited_teleport_assets_from_and_to_relay() {
	let amount = WESTEND_ED * 10;
	let native_asset: Assets = (Here, amount).into();

	test_relay_is_trusted_teleporter!(
		Westend,               // Origin
		vec![CoretimeWestend], // Destinations
		(native_asset, amount),
		limited_teleport_assets
	);

	test_parachain_is_trusted_teleporter_for_relay!(
		CoretimeWestend, // Origin
		Westend,         // Destination
		amount,
		limited_teleport_assets
	);
}

#[test]
fn teleport_via_transfer_assets_from_and_to_relay() {
	let amount = WESTEND_ED * 10;
	let native_asset: Assets = (Here, amount).into();

	test_relay_is_trusted_teleporter!(
		Westend,               // Origin
		vec![CoretimeWestend], // Destinations
		(native_asset, amount),
		transfer_assets
	);

	test_parachain_is_trusted_teleporter_for_relay!(
		CoretimeWestend, // Origin
		Westend,         // Destination
		amount,
		transfer_assets
	);
}

#[test]
fn teleport_via_limited_teleport_assets_from_coretime_to_asset_hub() {
	let amount = ASSET_HUB_WESTEND_ED * 100;
	let native_asset: Assets = (Parent, amount).into();

	test_parachain_is_trusted_teleporter!(
		CoretimeWestend,       // Origin
		vec![AssetHubWestend], // Destinations
		(native_asset, amount),
		limited_teleport_assets
	);
}

#[test]
fn teleport_via_transfer_assets_from_coretime_to_asset_hub() {
	let amount = ASSET_HUB_WESTEND_ED * 100;
	let native_asset: Assets = (Parent, amount).into();

	test_parachain_is_trusted_teleporter!(
		CoretimeWestend,       // Origin
		vec![AssetHubWestend], // Destinations
		(native_asset, amount),
		transfer_assets
	);
}

#[test]
fn teleport_via_limited_teleport_assets_from_asset_hub_to_coretime() {
	let amount = CORETIME_WESTEND_ED * 100;
	let native_asset: Assets = (Parent, amount).into();

	test_parachain_is_trusted_teleporter!(
		AssetHubWestend,       // Origin
		vec![CoretimeWestend], // Destinations
		(native_asset, amount),
		limited_teleport_assets
	);
}

#[test]
fn teleport_via_transfer_assets_from_asset_hub_to_coretime() {
	let amount = CORETIME_WESTEND_ED * 100;
	let native_asset: Assets = (Parent, amount).into();

	test_parachain_is_trusted_teleporter!(
		AssetHubWestend,       // Origin
		vec![CoretimeWestend], // Destinations
		(native_asset, amount),
		transfer_assets
	);
}

#[test]
fn accumulate_forward_coretime_transfers_native_to_asset_hub() {
	emulated_integration_tests_common::dap_helpers::test_accumulate_forward_transfers_to_asset_hub::<
		CoretimeWestend,
		AssetHubWestend,
	>();
}
