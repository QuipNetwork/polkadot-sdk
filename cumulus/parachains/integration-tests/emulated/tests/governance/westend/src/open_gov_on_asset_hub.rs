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
use crate::{common::*, imports::*};
use asset_hub_westend_runtime::governance::pallet_custom_origins::Origin;
use emulated_integration_tests_common::impls::Parachain;

#[test]
fn assethub_can_authorize_upgrade_for_itself() {
	let code_hash = [1u8; 32].into();
	type AssetHubRuntime = <AssetHubWestend as Chain>::Runtime;
	type AssetHubRuntimeCall = <AssetHubWestend as Chain>::RuntimeCall;
	type AssetHubRuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

	let authorize_upgrade =
		AssetHubRuntimeCall::Utility(pallet_utility::Call::<AssetHubRuntime>::force_batch {
			calls: vec![AssetHubRuntimeCall::System(frame_system::Call::authorize_upgrade {
				code_hash,
			})],
		});

	// bad origin
	let invalid_origin: AssetHubRuntimeOrigin = Origin::StakingAdmin.into();
	// ok origin
	let ok_origin: AssetHubRuntimeOrigin = Origin::WhitelistedCaller.into();

	// store preimage
	let call_hash = call_hash_of::<AssetHubWestend>(&authorize_upgrade);

	// Ok - dispatching non-whitelisted defers the call
	assert_ok!(dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
		authorize_upgrade.clone(),
		ok_origin.clone()
	));
	AssetHubWestend::execute_with(|| {
		assert!(pallet_whitelist::DeferredDispatch::<AssetHubRuntime>::contains_key(call_hash));
	});

	// whitelist
	collectives_send_whitelist(
		CollectivesWestend::sibling_location_of(<AssetHubWestend as Parachain>::para_id()),
		|| {
			AssetHubRuntimeCall::Whitelist(
				pallet_whitelist::Call::<AssetHubRuntime>::whitelist_call { call_hash },
			)
			.encode()
		},
	);

	// Err - when dispatch wrong origin
	assert_err!(
		dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
			authorize_upgrade.clone(),
			invalid_origin
		),
		DispatchError::BadOrigin
	);

	// check before
	AssetHubWestend::execute_with(|| {
		assert!(<AssetHubWestend as Chain>::System::authorized_upgrade().is_none())
	});

	// ok - authorized
	assert_ok!(dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
		authorize_upgrade,
		ok_origin
	));

	// check after - authorized
	AssetHubWestend::execute_with(|| {
		assert_eq!(
			<AssetHubWestend as Chain>::System::authorized_upgrade().unwrap().code_hash(),
			&code_hash
		)
	});
}

#[test]
fn assethub_can_authorize_upgrade_for_relay_chain() {
	let code_hash = [1u8; 32].into();
	type AssetHubRuntime = <AssetHubWestend as Chain>::Runtime;
	type AssetHubRuntimeCall = <AssetHubWestend as Chain>::RuntimeCall;
	type AssetHubRuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

	let authorize_upgrade =
		AssetHubRuntimeCall::Utility(pallet_utility::Call::<AssetHubRuntime>::force_batch {
			calls: vec![build_xcm_send_authorize_upgrade_call::<AssetHubWestend, Westend>(
				AssetHubWestend::parent_location(),
				&code_hash,
				None,
			)],
		});

	// bad origin
	let invalid_origin: AssetHubRuntimeOrigin = Origin::StakingAdmin.into();
	// ok origin
	let ok_origin: AssetHubRuntimeOrigin = Origin::WhitelistedCaller.into();

	let call_hash = call_hash_of::<AssetHubWestend>(&authorize_upgrade);

	// Ok - dispatching non-whitelisted defers the call
	assert_ok!(dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
		authorize_upgrade.clone(),
		ok_origin.clone()
	));
	AssetHubWestend::execute_with(|| {
		assert!(pallet_whitelist::DeferredDispatch::<AssetHubRuntime>::contains_key(call_hash));
	});

	// whitelist
	collectives_send_whitelist(
		CollectivesWestend::sibling_location_of(<AssetHubWestend as Parachain>::para_id()),
		|| {
			AssetHubRuntimeCall::Whitelist(
				pallet_whitelist::Call::<AssetHubRuntime>::whitelist_call { call_hash },
			)
			.encode()
		},
	);

	// Err - when dispatch wrong origin
	assert_err!(
		dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
			authorize_upgrade.clone(),
			invalid_origin
		),
		DispatchError::BadOrigin
	);

	// check before
	Westend::execute_with(|| assert!(<Westend as Chain>::System::authorized_upgrade().is_none()));

	// ok - authorized
	assert_ok!(dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
		authorize_upgrade,
		ok_origin
	));

	// check after - authorized
	Westend::execute_with(|| {
		assert_eq!(
			<Westend as Chain>::System::authorized_upgrade().unwrap().code_hash(),
			&code_hash
		)
	});
}

#[test]
fn assethub_can_authorize_upgrade_for_system_chains() {
	type AssetHubRuntime = <AssetHubWestend as Chain>::Runtime;
	type AssetHubRuntimeCall = <AssetHubWestend as Chain>::RuntimeCall;
	type AssetHubRuntimeOrigin = <AssetHubWestend as Chain>::RuntimeOrigin;

	let code_hash_bridge_hub = [2u8; 32].into();
	let code_hash_collectives = [3u8; 32].into();
	let code_hash_coretime = [4u8; 32].into();
	let code_hash_people = [5u8; 32].into();

	let authorize_upgrade =
		AssetHubRuntimeCall::Utility(pallet_utility::Call::<AssetHubRuntime>::force_batch {
			calls: vec![
				build_xcm_send_authorize_upgrade_call::<AssetHubWestend, BridgeHubWestend>(
					AssetHubWestend::sibling_location_of(BridgeHubWestend::para_id()),
					&code_hash_bridge_hub,
					None,
				),
				build_xcm_send_authorize_upgrade_call::<AssetHubWestend, CollectivesWestend>(
					AssetHubWestend::sibling_location_of(CollectivesWestend::para_id()),
					&code_hash_collectives,
					None,
				),
				build_xcm_send_authorize_upgrade_call::<AssetHubWestend, CoretimeWestend>(
					AssetHubWestend::sibling_location_of(CoretimeWestend::para_id()),
					&code_hash_coretime,
					None,
				),
				build_xcm_send_authorize_upgrade_call::<AssetHubWestend, PeopleWestend>(
					AssetHubWestend::sibling_location_of(PeopleWestend::para_id()),
					&code_hash_people,
					None,
				),
			],
		});

	// bad origin
	let invalid_origin: AssetHubRuntimeOrigin = Origin::StakingAdmin.into();
	// ok origin
	let ok_origin: AssetHubRuntimeOrigin = Origin::WhitelistedCaller.into();

	let call_hash = call_hash_of::<AssetHubWestend>(&authorize_upgrade);

	// Ok - dispatching non-whitelisted defers the call
	assert_ok!(dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
		authorize_upgrade.clone(),
		ok_origin.clone()
	));
	AssetHubWestend::execute_with(|| {
		assert!(pallet_whitelist::DeferredDispatch::<AssetHubRuntime>::contains_key(call_hash));
	});

	// whitelist
	collectives_send_whitelist(
		CollectivesWestend::sibling_location_of(<AssetHubWestend as Parachain>::para_id()),
		|| {
			AssetHubRuntimeCall::Whitelist(
				pallet_whitelist::Call::<AssetHubRuntime>::whitelist_call { call_hash },
			)
			.encode()
		},
	);

	// Err - when dispatch wrong origin
	assert_err!(
		dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
			authorize_upgrade.clone(),
			invalid_origin
		),
		DispatchError::BadOrigin
	);

	// check before
	BridgeHubWestend::execute_with(|| {
		assert!(<BridgeHubWestend as Chain>::System::authorized_upgrade().is_none())
	});
	CollectivesWestend::execute_with(|| {
		assert!(<CollectivesWestend as Chain>::System::authorized_upgrade().is_none())
	});
	CoretimeWestend::execute_with(|| {
		assert!(<CoretimeWestend as Chain>::System::authorized_upgrade().is_none())
	});
	PeopleWestend::execute_with(|| {
		assert!(<PeopleWestend as Chain>::System::authorized_upgrade().is_none())
	});

	// ok - authorized
	assert_ok!(dispatch_whitelisted_call_with_preimage::<AssetHubWestend>(
		authorize_upgrade,
		ok_origin
	));

	// check after - authorized
	BridgeHubWestend::execute_with(|| {
		assert_eq!(
			<BridgeHubWestend as Chain>::System::authorized_upgrade().unwrap().code_hash(),
			&code_hash_bridge_hub
		)
	});
	CollectivesWestend::execute_with(|| {
		assert_eq!(
			<CollectivesWestend as Chain>::System::authorized_upgrade().unwrap().code_hash(),
			&code_hash_collectives
		)
	});
	CoretimeWestend::execute_with(|| {
		assert_eq!(
			<CoretimeWestend as Chain>::System::authorized_upgrade().unwrap().code_hash(),
			&code_hash_coretime
		)
	});
	PeopleWestend::execute_with(|| {
		assert_eq!(
			<PeopleWestend as Chain>::System::authorized_upgrade().unwrap().code_hash(),
			&code_hash_people
		)
	});
}
