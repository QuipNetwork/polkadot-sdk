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
// along with Polkadot. If not, see <http://www.gnu.org/licenses/>.

//! Tests for the Westend Runtime Configuration

use std::collections::HashSet;

use crate::{xcm_config::LocationConverter, *};
use frame_support::{
	assert_ok,
	traits::{
		fungible::{Inspect, Mutate},
		WhitelistedStorageKeys,
	},
};
use sp_core::{crypto::Ss58Codec, hexdisplay::HexDisplay};
use sp_keyring::Sr25519Keyring::{self, Alice};
use sp_runtime::{generic::Era, traits::AccountIdConversion};
use xcm_runtime_apis::conversions::LocationToAccountHelper;

#[test]
fn remove_keys_weight_is_sensible() {
	use polkadot_runtime_common::crowdloan::WeightInfo;
	let max_weight = <Runtime as crowdloan::Config>::WeightInfo::refund(RemoveKeysLimit::get());
	// Max remove keys limit should be no more than half the total block weight.
	assert!((max_weight * 2).all_lt(BlockWeights::get().max_block));
}

#[test]
fn sample_size_is_sensible() {
	use polkadot_runtime_common::auctions::WeightInfo;
	// Need to clean up all samples at the end of an auction.
	let samples: BlockNumber = EndingPeriod::get() / SampleLength::get();
	let max_weight: frame_support::weights::Weight =
		RocksDbWeight::get().reads_writes(samples.into(), samples.into());
	// Max sample cleanup should be no more than half the total block weight.
	assert!((max_weight * 2).all_lt(BlockWeights::get().max_block));
	assert!((<Runtime as auctions::Config>::WeightInfo::on_initialize() * 2)
		.all_lt(BlockWeights::get().max_block));
}

#[test]
fn call_size() {
	RuntimeCall::assert_size_under(256);
}

#[test]
fn sanity_check_teleport_assets_weight() {
	// This test sanity checks that at least 50 teleports can exist in a block.
	// Usually when XCM runs into an issue, it will return a weight of `Weight::MAX`,
	// so this test will certainly ensure that this problem does not occur.
	use frame_support::dispatch::GetDispatchInfo;
	let weight = pallet_xcm::Call::<Runtime>::limited_teleport_assets {
		dest: Box::new(Here.into()),
		beneficiary: Box::new(Here.into()),
		assets: Box::new((Here, 200_000).into()),
		fee_asset_item: 0,
		weight_limit: Unlimited,
	}
	.get_dispatch_info()
	.call_weight;

	assert!((weight * 50).all_lt(BlockWeights::get().max_block));
}

#[test]
fn check_whitelist() {
	let whitelist: HashSet<String> = AllPalletsWithSystem::whitelisted_storage_keys()
		.iter()
		.map(|e| HexDisplay::from(&e.key).to_string())
		.collect();

	// Block number
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef702a5c1b19ab7a04f536c519aca4983ac"));
	// Total issuance
	assert!(whitelist.contains("c2261276cc9d1f8598ea4b6a74b15c2f57c875e4cff74148e4628f264b974c80"));
	// Execution phase
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef7ff553b5a9862a516939d82b3d3d8661a"));
	// Event count
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef70a98fdbe9ce6c55837576c60c7af3850"));
	// System events
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef780d41e5e16056765bc8461851072c9d7"));
	// Configuration ActiveConfig
	assert!(whitelist.contains("06de3d8a54d27e44a9d5ce189618f22db4b49d95320d9021994c850f25b8e385"));
	// XcmPallet VersionDiscoveryQueue
	assert!(whitelist.contains("1405f2411d0af5a7ff397e7c9dc68d194a222ba0333561192e474c59ed8e30e1"));
	// XcmPallet SafeXcmVersion
	assert!(whitelist.contains("1405f2411d0af5a7ff397e7c9dc68d196323ae84c43568be0d1394d5d0d522c4"));
}

#[cfg(all(test, feature = "try-runtime"))]
mod remote_tests {
	use super::*;
	use frame_try_runtime::{runtime_decl_for_try_runtime::TryRuntime, UpgradeCheckSelect};
	use remote_externalities::{Builder, Mode, OfflineConfig, OnlineConfig, SnapshotConfig};
	use std::env::var;

	#[tokio::test]
	async fn run_migrations() {
		if var("RUN_MIGRATION_TESTS").is_err() {
			return;
		}

		sp_tracing::try_init_simple();
		let transport_uri = var("WS").unwrap_or("wss://westend-rpc.polkadot.io:443".to_string());
		let maybe_state_snapshot: Option<SnapshotConfig> = var("SNAP").map(|s| s.into()).ok();
		let mut ext = Builder::<Block>::default()
			.mode(if let Some(state_snapshot) = maybe_state_snapshot {
				Mode::OfflineOrElseOnline(
					OfflineConfig { state_snapshot: state_snapshot.clone() },
					OnlineConfig {
						transport_uris: vec![transport_uri.clone()],
						state_snapshot: Some(state_snapshot),
						..Default::default()
					},
				)
			} else {
				Mode::Online(OnlineConfig {
					transport_uris: vec![transport_uri],
					..Default::default()
				})
			})
			.build()
			.await
			.unwrap();
		ext.execute_with(|| Runtime::on_runtime_upgrade(UpgradeCheckSelect::PreAndPost));
	}
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
			description: "DescribeTerminus Child",
			location: Location::new(0, [Parachain(1111)]),
			expected_account_id_str: "5Ec4AhP4h37t7TFsAZ4HhFq6k92usAAJDUC3ADSZ4H4Acru3",
		},
		// DescribePalletTerminal
		TestCase {
			description: "DescribePalletTerminal Child",
			location: Location::new(0, [Parachain(1111), PalletInstance(50)]),
			expected_account_id_str: "5FjEBrKn3STAFsZpQF4jzwxUYHNGnNgzdZqSQfTzeJ82XKp6",
		},
		// DescribeAccountId32Terminal
		TestCase {
			description: "DescribeAccountId32Terminal Child",
			location: Location::new(
				0,
				[Parachain(1111), AccountId32 { network: None, id: AccountId::from(Alice).into() }],
			),
			expected_account_id_str: "5EEMro9RRDpne4jn9TuD7cTB6Amv1raVZ3xspSkqb2BF3FJH",
		},
		// DescribeAccountKey20Terminal
		TestCase {
			description: "DescribeAccountKey20Terminal Child",
			location: Location::new(
				0,
				[Parachain(1111), AccountKey20 { network: None, key: [0u8; 20] }],
			),
			expected_account_id_str: "5HohjXdjs6afcYcgHHSstkrtGfxgfGKsnZ1jtewBpFiGu4DL",
		},
		// DescribeTreasuryVoiceTerminal
		TestCase {
			description: "DescribeTreasuryVoiceTerminal Child",
			location: Location::new(
				0,
				[Parachain(1111), Plurality { id: BodyId::Treasury, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5GenE4vJgHvwYVcD6b4nBvH5HNY4pzpVHWoqwFpNMFT7a2oX",
		},
		// DescribeBodyTerminal
		TestCase {
			description: "DescribeBodyTerminal Child",
			location: Location::new(
				0,
				[Parachain(1111), Plurality { id: BodyId::Unit, part: BodyPart::Voice }],
			),
			expected_account_id_str: "5DPgGBFTTYm1dGbtB1VWHJ3T3ScvdrskGGx6vSJZNP1WNStV",
		},
	];

	for tc in test_cases {
		let expected =
			AccountId::from_string(tc.expected_account_id_str).expect("Invalid AccountId string");

		let got = LocationToAccountHelper::<AccountId, LocationConverter>::convert_location(
			tc.location.into(),
		)
		.unwrap();

		assert_eq!(got, expected, "{}", tc.description);
	}
}

fn new_test_ext() -> sp_io::TestExternalities {
	frame_system::GenesisConfig::<Runtime>::default()
		.build_storage()
		.unwrap()
		.into()
}

fn construct_extrinsic(sender: Sr25519Keyring, call: RuntimeCall) -> UncheckedExtrinsic {
	let account_id = AccountId::from(sender);
	let tx_ext: TxExtension = (
		frame_system::AuthorizeCall::<Runtime>::new(),
		frame_system::CheckNonZeroSender::<Runtime>::new(),
		frame_system::CheckSpecVersion::<Runtime>::new(),
		frame_system::CheckTxVersion::<Runtime>::new(),
		frame_system::CheckGenesis::<Runtime>::new(),
		frame_system::CheckMortality::from(Era::immortal()),
		frame_system::CheckNonce::<Runtime>::from(
			frame_system::Pallet::<Runtime>::account(&account_id).nonce,
		),
		frame_system::CheckWeight::<Runtime>::new(),
		pallet_transaction_payment::ChargeTransactionPayment::<Runtime>::from(0),
		frame_metadata_hash_extension::CheckMetadataHash::new(false),
		frame_system::WeightReclaim::<Runtime>::new(),
	);
	let payload = sp_runtime::generic::SignedPayload::new(call.clone(), tx_ext.clone()).unwrap();
	let signature = payload.using_encoded(|e| sender.sign(e));
	UncheckedExtrinsic::new_signed(call, account_id.into(), Signature::Sr25519(signature), tx_ext)
}

#[test]
fn tx_fees_go_to_accumulation_account() {
	new_test_ext().execute_with(|| {
		let alice = AccountId::from(Sr25519Keyring::Alice);
		let accumulation_account =
			pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
		let ed = ExistentialDeposit::get();

		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&alice, 100 * ed));
		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&accumulation_account, ed));

		let alice_before = <Balances as Inspect<AccountId>>::balance(&alice);
		let accumulation_account_before =
			<Balances as Inspect<AccountId>>::balance(&accumulation_account);
		let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
		let xt = construct_extrinsic(Sr25519Keyring::Alice, call);
		assert_ok!(Executive::apply_extrinsic(xt).unwrap());

		let alice_after = <Balances as Inspect<AccountId>>::balance(&alice);
		let fee_paid = alice_before - alice_after;
		assert!(fee_paid > 0, "a fee should have been paid");

		let accumulation_account_after =
			<Balances as Inspect<AccountId>>::balance(&accumulation_account);
		let issuance_after = <Balances as Inspect<AccountId>>::total_issuance();

		assert_eq!(accumulation_account_after, accumulation_account_before + fee_paid);
		assert_eq!(issuance_before, issuance_after);
	});
}

#[test]
fn dust_removal_goes_to_accumulation_account() {
	new_test_ext().execute_with(|| {
		let alice: AccountId = Sr25519Keyring::Alice.into();
		let bob: AccountId = Sr25519Keyring::Bob.into();
		let accumulation_account =
			pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account();
		let ed = ExistentialDeposit::get();
		let dust = ed / 2;

		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&bob, ed + dust));
		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&alice, 100 * ed));
		assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&accumulation_account, ed));

		let accumulation_account_before =
			<Balances as Inspect<AccountId>>::balance(&accumulation_account);

		// Transfer ED away from bob, leaving dust < ED → account reaped.
		assert_ok!(Balances::transfer_allow_death(
			RuntimeOrigin::signed(bob.clone()),
			alice.into(),
			ed,
		));

		let accumulation_account_after =
			<Balances as Inspect<AccountId>>::balance(&accumulation_account);
		assert_eq!(accumulation_account_after, accumulation_account_before + dust);
		assert_eq!(<Balances as Inspect<AccountId>>::balance(&bob), 0);
	});
}

#[test]
fn accumulate_forward_account_matches_constant() {
	assert_eq!(
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account(),
		AccumulateForwardPalletId::get().into_account_truncating()
	);
}

/// Unit tests for `migrations::DrainLegacyTreasuryToAccumulationAccount`.
mod drain_legacy_treasury_migration {
	use super::*;
	use crate::migrations::DrainLegacyTreasuryToAccumulationAccount;
	use frame_support::{traits::OnRuntimeUpgrade, PalletId};

	const LEGACY_TREASURY_PALLET_ID: PalletId = PalletId(*b"py/trsry");

	fn legacy_account() -> AccountId {
		LEGACY_TREASURY_PALLET_ID.into_account_truncating()
	}

	fn accumulation_account() -> AccountId {
		pallet_accumulate_and_forward::Pallet::<Runtime>::accumulation_account()
	}

	#[test]
	fn zero_source_is_a_no_op() {
		new_test_ext().execute_with(|| {
			assert_eq!(<Balances as Inspect<AccountId>>::balance(&legacy_account()), 0);
			let accum_before = <Balances as Inspect<AccountId>>::balance(&accumulation_account());
			let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

			DrainLegacyTreasuryToAccumulationAccount::on_runtime_upgrade();

			assert_eq!(
				<Balances as Inspect<AccountId>>::balance(&accumulation_account()),
				accum_before
			);
			assert_eq!(<Balances as Inspect<AccountId>>::total_issuance(), issuance_before);
		});
	}

	#[test]
	fn only_ed_in_source_is_also_a_no_op() {
		new_test_ext().execute_with(|| {
			let ed = ExistentialDeposit::get();
			// Fund with exactly ED — reducible_balance(Preserve) returns 0.
			assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&legacy_account(), ed));
			let accum_before = <Balances as Inspect<AccountId>>::balance(&accumulation_account());
			let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

			DrainLegacyTreasuryToAccumulationAccount::on_runtime_upgrade();

			assert_eq!(
				<Balances as Inspect<AccountId>>::balance(&accumulation_account()),
				accum_before
			);
			assert_eq!(<Balances as Inspect<AccountId>>::total_issuance(), issuance_before);
		});
	}

	#[test]
	fn drains_reducible_balance_to_accumulation_account() {
		new_test_ext().execute_with(|| {
			let ed = ExistentialDeposit::get();
			// reducible = 90 * ED (ED itself kept by Preservation::Preserve).
			let extra = 90 * ed;
			assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&legacy_account(), ed + extra));
			let accum_before = <Balances as Inspect<AccountId>>::balance(&accumulation_account());
			let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

			DrainLegacyTreasuryToAccumulationAccount::on_runtime_upgrade();

			assert_eq!(<Balances as Inspect<AccountId>>::balance(&legacy_account()), ed);
			assert_eq!(
				<Balances as Inspect<AccountId>>::balance(&accumulation_account()),
				accum_before + extra
			);
			assert_eq!(<Balances as Inspect<AccountId>>::total_issuance(), issuance_before);
		});
	}

	#[test]
	fn idempotent_second_run_is_a_no_op() {
		new_test_ext().execute_with(|| {
			let ed = ExistentialDeposit::get();
			assert_ok!(<Balances as Mutate<AccountId>>::mint_into(&legacy_account(), ed + 50 * ed));

			DrainLegacyTreasuryToAccumulationAccount::on_runtime_upgrade();
			let accum_after_first =
				<Balances as Inspect<AccountId>>::balance(&accumulation_account());

			// Second run: legacy reducible is now 0 → early return.
			DrainLegacyTreasuryToAccumulationAccount::on_runtime_upgrade();
			assert_eq!(
				<Balances as Inspect<AccountId>>::balance(&accumulation_account()),
				accum_after_first
			);
		});
	}

	#[test]
	fn total_issuance_invariant_holds() {
		new_test_ext().execute_with(|| {
			let ed = ExistentialDeposit::get();
			assert_ok!(<Balances as Mutate<AccountId>>::mint_into(
				&legacy_account(),
				ed + 200 * ed
			));
			let issuance_before = <Balances as Inspect<AccountId>>::total_issuance();

			DrainLegacyTreasuryToAccumulationAccount::on_runtime_upgrade();

			assert_eq!(<Balances as Inspect<AccountId>>::total_issuance(), issuance_before);
		});
	}
}
