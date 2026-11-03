// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

#![cfg(test)]

use codec::Encode;
use coretime_westend_runtime::{
	xcm_config::{GovernanceLocation, LocationToAccountId},
	AccumulateForward, Balances, Block, Executive, ExistentialDeposit, Runtime, RuntimeCall,
	RuntimeOrigin, TxExtension, UncheckedExtrinsic,
};
use frame_support::{assert_err, assert_ok, traits::fungible::Inspect};
use parachains_common::{AccountId, AuraId, Signature};
use parachains_runtimes_test_utils::{ExtBuilder, GovernanceOrigin};
use sp_core::crypto::Ss58Codec;
use sp_keyring::Sr25519Keyring;
use sp_runtime::{generic::Era, AccountId32, Either};
use testnet_parachains_constants::westend::fee::WeightToFee;
use xcm::latest::prelude::*;
use xcm_runtime_apis::conversions::LocationToAccountHelper;

const ALICE: [u8; 32] = [1u8; 32];

fn construct_extrinsic(
	sender: sp_keyring::Sr25519Keyring,
	call: RuntimeCall,
) -> UncheckedExtrinsic {
	let account_id = AccountId32::from(sender.public());
	let tx_ext: TxExtension = (
		frame_system::AuthorizeCall::<Runtime>::new(),
		frame_system::CheckNonZeroSender::<Runtime>::new(),
		frame_system::CheckSpecVersion::<Runtime>::new(),
		frame_system::CheckTxVersion::<Runtime>::new(),
		frame_system::CheckGenesis::<Runtime>::new(),
		frame_system::CheckEra::<Runtime>::from(Era::immortal()),
		frame_system::CheckNonce::<Runtime>::from(
			frame_system::Pallet::<Runtime>::account(&account_id).nonce,
		),
		frame_system::CheckWeight::<Runtime>::new(),
		pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(0),
		frame_metadata_hash_extension::CheckMetadataHash::new(false),
	)
		.into();
	let payload = sp_runtime::generic::SignedPayload::new(call.clone(), tx_ext.clone()).unwrap();
	let signature = payload.using_encoded(|e| sender.sign(e));
	UncheckedExtrinsic::new_signed(call, account_id.into(), Signature::Sr25519(signature), tx_ext)
}

fn collator_session_keys() -> parachains_runtimes_test_utils::CollatorSessionKeys<Runtime> {
	parachains_runtimes_test_utils::CollatorSessionKeys::new(
		AccountId::from(Sr25519Keyring::Alice),
		AccountId::from(Sr25519Keyring::Alice),
		coretime_westend_runtime::SessionKeys {
			aura: AuraId::from(Sr25519Keyring::Alice.public()),
		},
	)
}

#[test]
fn location_conversion_works() {
	// the purpose of hardcoded values is to catch an unintended location conversion logic change.
	struct TestCase {
		description: &'static str,
		location: Location,
		expected_account_id_str: &'static str,
	}

	let test_cases = vec![
		// DescribeTerminus
		TestCase {
			description: "DescribeTerminus Parent",
			location: Location::new(1, Here),
			expected_account_id_str: "5Dt6dpkWPwLaH4BBCKJwjiWrFVAGyYk3tLUabvyn4v7KtESG",
		},
		TestCase {
			description: "DescribeTerminus Sibling",
			location: Location::new(1, [Parachain(1111)]),
			expected_account_id_str: "5Eg2fnssmmJnF3z1iZ1NouAuzciDaaDQH7qURAy3w15jULDk",
		},
		// DescribePalletTerminal
		TestCase {
			description: "DescribePalletTerminal Parent",
			location: Location::new(1, [PalletInstance(50)]),
			expected_account_id_str: "5CnwemvaAXkWFVwibiCvf2EjqwiqBi29S5cLLydZLEaEw6jZ",
		},
		TestCase {
			description: "DescribePalletTerminal Sibling",
			location: Location::new(1, [Parachain(1111), PalletInstance(50)]),
			expected_account_id_str: "5GFBgPjpEQPdaxEnFirUoa51u5erVx84twYxJVuBRAT2UP2g",
		},
		// DescribeAccountId32Terminal
		TestCase {
			description: "DescribeAccountId32Terminal Parent",
			location: Location::new(
				1,
				[Junction::AccountId32 { network: None, id: AccountId::from(ALICE).into() }],
			),
			expected_account_id_str: "5DN5SGsuUG7PAqFL47J9meViwdnk9AdeSWKFkcHC45hEzVz4",
		},
		TestCase {
			description: "DescribeAccountId32Terminal Sibling",
			location: Location::new(
				1,
				[
					Parachain(1111),
					Junction::AccountId32 { network: None, id: AccountId::from(ALICE).into() },
				],
			),
			expected_account_id_str: "5DGRXLYwWGce7wvm14vX1Ms4Vf118FSWQbJkyQigY2pfm6bg",
		},
		// DescribeAccountKey20Terminal
		TestCase {
			description: "DescribeAccountKey20Terminal Parent",
			location: Location::new(1, [AccountKey20 { network: None, key: [0u8; 20] }]),
			expected_account_id_str: "5F5Ec11567pa919wJkX6VHtv2ZXS5W698YCW35EdEbrg14cg",
		},
		TestCase {
			description: "DescribeAccountKey20Terminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), AccountKey20 { network: None, key: [0u8; 20] }],
			),
			expected_account_id_str: "5CB2FbUds2qvcJNhDiTbRZwiS3trAy6ydFGMSVutmYijpPAg",
		},
		// DescribeTreasuryVoiceTerminal
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Parent",
			location: Location::new(1, [Plurality { id: BodyId::Treasury, part: BodyPart::Voice }]),
			expected_account_id_str: "5CUjnE2vgcUCuhxPwFoQ5r7p1DkhujgvMNDHaF2bLqRp4D5F",
		},
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), Plurality { id: BodyId::Treasury, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5G6TDwaVgbWmhqRUKjBhRRnH4ry9L9cjRymUEmiRsLbSE4gB",
		},
		// DescribeBodyTerminal
		TestCase {
			description: "DescribeBodyTerminal Parent",
			location: Location::new(1, [Plurality { id: BodyId::Unit, part: BodyPart::Voice }]),
			expected_account_id_str: "5EBRMTBkDisEXsaN283SRbzx9Xf2PXwUxxFCJohSGo4jYe6B",
		},
		TestCase {
			description: "DescribeBodyTerminal Sibling",
			location: Location::new(
				1,
				[Parachain(1111), Plurality { id: BodyId::Unit, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5DBoExvojy8tYnHgLL97phNH975CyT45PWTZEeGoBZfAyRMH",
		},
	];

	for tc in test_cases {
		let expected =
			AccountId::from_string(tc.expected_account_id_str).expect("Invalid AccountId string");

		let got = LocationToAccountHelper::<AccountId, LocationToAccountId>::convert_location(
			tc.location.into(),
		)
		.unwrap();

		assert_eq!(got, expected, "{}", tc.description);
	}
}

#[test]
fn xcm_payment_api_works() {
	parachains_runtimes_test_utils::test_cases::xcm_payment_api_with_native_token_works::<
		Runtime,
		RuntimeCall,
		RuntimeOrigin,
		Block,
		WeightToFee,
	>();
}

#[test]
fn governance_authorize_upgrade_works() {
	use westend_runtime_constants::system_parachain::{ASSET_HUB_ID, COLLECTIVES_ID};

	// no - random para
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::Location(Location::new(1, Parachain(12334)))),
		Either::Right(InstructionError { index: 0, error: XcmError::Barrier })
	);
	// ok - AssetHub
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Location(Location::new(1, Parachain(ASSET_HUB_ID)))));
	// no - Collectives (passes barrier as system parachain, but not root)
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::Location(Location::new(1, Parachain(COLLECTIVES_ID)))),
		Either::Right(InstructionError { index: 1, error: XcmError::BadOrigin })
	);
	// no - Collectives Voice of Fellows plurality
	assert_err!(
		parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
			Runtime,
			RuntimeOrigin,
		>(GovernanceOrigin::LocationAndDescendOrigin(
			Location::new(1, Parachain(COLLECTIVES_ID)),
			Plurality { id: BodyId::Technical, part: BodyPart::Voice }.into()
		)),
		Either::Right(InstructionError { index: 2, error: XcmError::BadOrigin })
	);

	// ok - relaychain
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Location(Location::parent())));

	// ok - governance location
	assert_ok!(parachains_runtimes_test_utils::test_cases::can_governance_authorize_upgrade::<
		Runtime,
		RuntimeOrigin,
	>(GovernanceOrigin::Location(GovernanceLocation::get())));
}

#[test]
fn tx_fees_go_to_accumulation_account() {
	let alice = AccountId::from(Sr25519Keyring::Alice);
	let accumulation_account =
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	let ed = ExistentialDeposit::get();

	ExtBuilder::<Runtime>::default()
		.with_collators(collator_session_keys().collators())
		.with_session_keys(collator_session_keys().session_keys())
		.with_balances(vec![(alice.clone(), 100 * ed), (accumulation_account.clone(), ed)])
		.with_para_id(1005.into())
		.build()
		.execute_with(|| {
			let alice_before = <Balances as Inspect<AccountId>>::balance(&alice);
			let accumulation_before =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

			let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
			let xt = construct_extrinsic(Sr25519Keyring::Alice, call);
			assert_ok!(Executive::apply_extrinsic(xt).unwrap());

			let alice_after = <Balances as Inspect<AccountId>>::balance(&alice);
			let fee_paid = alice_before - alice_after;
			assert!(fee_paid > 0, "a fee should have been paid");

			let accumulation_after =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			let issuance_after = <Balances as Inspect<AccountId>>::total_issuance();

			assert_eq!(accumulation_after, accumulation_before + fee_paid);
			assert_eq!(issuance_before, issuance_after);
		});
}

#[test]
fn dust_removal_goes_to_accumulation_account() {
	let alice = AccountId::from(Sr25519Keyring::Alice);
	let bob = AccountId::from(Sr25519Keyring::Bob);
	let accumulation_account =
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	let ed = ExistentialDeposit::get();
	let dust = ed / 2;

	ExtBuilder::<Runtime>::default()
		.with_collators(collator_session_keys().collators())
		.with_session_keys(collator_session_keys().session_keys())
		.with_balances(vec![
			(alice.clone(), 100 * ed),
			(bob.clone(), ed + dust),
			(accumulation_account.clone(), ed),
		])
		.with_para_id(1005.into())
		.build()
		.execute_with(|| {
			let accumulation_before =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);

			// When: transfer ED away from bob, leaving dust < ED behind → account reaped.
			assert_ok!(Balances::transfer_allow_death(
				RuntimeOrigin::signed(bob.clone()),
				alice.clone().into(),
				ed,
			));

			// Then: bob's account is killed, dust goes to accumulation account.
			let accumulation_after =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			assert_eq!(
				accumulation_after,
				accumulation_before + dust,
				"accumulation account should receive dust"
			);
			assert_eq!(<Balances as Inspect<AccountId>>::balance(&bob), 0, "bob should be reaped");
		});
}

#[test]
fn coretime_revenue_goes_to_accumulation_account() {
	use frame_support::traits::{fungible::Balanced, tokens::imbalance::OnUnbalanced};

	let accumulation_account =
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
	let ed = ExistentialDeposit::get();
	let revenue = 1_000_000_000u128;

	ExtBuilder::<Runtime>::default()
		.with_collators(collator_session_keys().collators())
		.with_session_keys(collator_session_keys().session_keys())
		.with_balances(vec![(accumulation_account.clone(), ed)])
		.with_para_id(1005.into())
		.build()
		.execute_with(|| {
			let accumulation_before =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);

			// When: simulate coretime revenue via OnUnbalanced with an issued credit.
			let credit = <Balances as Balanced<AccountId>>::issue(revenue);
			<AccumulateForward as OnUnbalanced<_>>::on_unbalanced(credit);

			// Then: accumulation account receives the revenue.
			let accumulation_after =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account);
			assert_eq!(
				accumulation_after,
				accumulation_before + revenue,
				"accumulation account should receive coretime revenue"
			);
		});
}
