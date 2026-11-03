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
use frame_support::{
	assert_ok, instances::Instance1, sp_runtime::traits::Dispatchable, traits::fungible::Inspect,
};
use polkadot_runtime_common::impls::VersionedLocatableAsset;
use westend_runtime_constants::currency::UNITS;
use xcm_executor::traits::ConvertLocation;

/// Fellowship treasury can spend funds on Asset Hub to a beneficiary.
#[test]
fn fellowship_treasury_spend() {
	// target fellowship balance on Asset Hub in WNDs.
	let fellowship_treasury_balance = 1_000_000 * UNITS;
	// fellowship first spend balance in WNDs.
	let fellowship_spend_balance = 10_000 * UNITS;

	let init_alice_balance = AssetHubWestend::execute_with(|| {
		<<AssetHubWestend as AssetHubWestendPallet>::Balances as Inspect<_>>::balance(
			&AssetHubWestend::account_id_of(ALICE),
		)
	});

	// Directly fund the fellowship treasury account on Asset Hub.
	AssetHubWestend::execute_with(|| {
		type Balances = <AssetHubWestend as AssetHubWestendPallet>::Balances;

		let fellowship_treasury_location: Location =
			Location::new(1, [Parachain(1001), PalletInstance(65)]);
		let fellowship_treasury_account =
			AssetHubLocationToAccountId::convert_location(&fellowship_treasury_location).unwrap();

		assert_ok!(Balances::force_set_balance(
			<AssetHubWestend as Chain>::RuntimeOrigin::root(),
			fellowship_treasury_account.into(),
			fellowship_treasury_balance,
		));
	});

	CollectivesWestend::execute_with(|| {
		type RuntimeEvent = <CollectivesWestend as Chain>::RuntimeEvent;
		type RuntimeCall = <CollectivesWestend as Chain>::RuntimeCall;
		type RuntimeOrigin = <CollectivesWestend as Chain>::RuntimeOrigin;
		type Runtime = <CollectivesWestend as Chain>::Runtime;
		type FellowshipTreasury =
			<CollectivesWestend as CollectivesWestendPallet>::FellowshipTreasury;

		// Fund Alice account from Fellowship Treasury.

		let fellows_origin: RuntimeOrigin =
			collectives_fellowship::pallet_fellowship_origins::Origin::Fellows.into();
		let asset_hub_location: Location = (Parent, Parachain(1000)).into();
		let native_asset = Location::parent();

		let alice_location: Location = [Junction::AccountId32 {
			network: None,
			id: CollectivesWestend::account_id_of(ALICE).into(),
		}]
		.into();

		let fellowship_treasury_spend_call =
			RuntimeCall::FellowshipTreasury(pallet_treasury::Call::<Runtime, Instance1>::spend {
				asset_kind: bx!(VersionedLocatableAsset::from((
					asset_hub_location,
					native_asset.into()
				))),
				amount: fellowship_spend_balance,
				beneficiary: bx!(VersionedLocation::from(alice_location)),
				valid_from: None,
			});

		assert_ok!(fellowship_treasury_spend_call.dispatch(fellows_origin));

		// Claim the spend.

		let alice_signed = RuntimeOrigin::signed(CollectivesWestend::account_id_of(ALICE));
		assert_ok!(FellowshipTreasury::payout(alice_signed.clone(), 0));

		assert_expected_events!(
			CollectivesWestend,
			vec![
				RuntimeEvent::FellowshipTreasury(pallet_treasury::Event::AssetSpendApproved { .. }) => {},
				RuntimeEvent::FellowshipTreasury(pallet_treasury::Event::Paid { .. }) => {},
			]
		);
	});

	AssetHubWestend::execute_with(|| {
		type RuntimeEvent = <AssetHubWestend as Chain>::RuntimeEvent;
		type Balances = <AssetHubWestend as AssetHubWestendPallet>::Balances;

		// Ensure that the funds deposited to Alice account.

		let alice_account = AssetHubWestend::account_id_of(ALICE);
		assert_eq!(
			<Balances as Inspect<_>>::balance(&alice_account),
			fellowship_spend_balance + init_alice_balance
		);

		// Assert events on Asset Hub triggered by the fellowship treasury payout:
		// 1. fellowship treasury transferred funds to Alice;
		// 2. payment status response sent back to Collectives chain;
		// 3. inbound XCM from Collectives processed successfully;
		assert_expected_events!(
			AssetHubWestend,
			vec![
				RuntimeEvent::Balances(pallet_balances::Event::Transfer { .. }) => {},
				RuntimeEvent::XcmpQueue(cumulus_pallet_xcmp_queue::Event::XcmpMessageSent { .. }) => {},
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: true ,.. }) => {},
			]
		);
	});
}
