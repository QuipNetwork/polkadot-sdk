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

#![cfg(test)]

pub(crate) mod assets_transfer;

use crate::{
	aliasers_footprint,
	migration::data::NeedsMigration,
	mock::*,
	pallet::{LockedFungibles, RemoteLockedFungibles, SupportedVersion},
	xcm_helpers::find_xcm_sent_message_id,
	AssetTraps, AuthorizedAliasers, Config, CurrentMigration, Error, ExecuteControllerWeightInfo,
	LatestVersionedLocation, MaxAuthorizedAliases, Pallet, Queries, QueryStatus, RecordedXcm,
	RemoteLockedFungibleRecord, ShouldRecordXcm, VersionDiscoveryQueue, VersionMigrationStage,
	VersionNotifiers, VersionNotifyTargets, WeightInfo,
};
use bounded_collections::BoundedVec;
use frame_support::{
	assert_err_ignore_postinfo, assert_noop, assert_ok, assert_storage_noop,
	traits::{ContainsPair, Currency, Hooks},
	weights::Weight,
};
use polkadot_parachain_primitives::primitives::Id as ParaId;
use sp_runtime::{
	traits::{AccountIdConversion, BlakeTwo256, BlockNumberProvider, Hash},
	SaturatedConversion, TokenError,
};
use xcm::{
	latest::{QueryResponseInfo, MAX_ITEMS_IN_ASSETS},
	prelude::*,
};
use xcm_builder::AllowKnownQueryResponses;
use xcm_executor::{
	traits::{Properties, QueryHandler, QueryResponseStatus, ShouldExecute},
	XcmExecutor,
};
use xcm_simulator::fake_message_hash;

const ALICE: AccountId = AccountId::new([0u8; 32]);
const BOB: AccountId = AccountId::new([1u8; 32]);
const INITIAL_BALANCE: u128 = 1000;
const SEND_AMOUNT: u128 = 10;
const FEE_AMOUNT: u128 = 2;

#[test]
fn report_outcome_notify_works() {
	let balances = vec![
		(ALICE, INITIAL_BALANCE),
		(ParaId::from(OTHER_PARA_ID).into_account_truncating(), INITIAL_BALANCE),
	];
	let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
	let mut message = Xcm(vec![TransferAsset {
		assets: (Here, SEND_AMOUNT).into(),
		beneficiary: sender.clone(),
	}]);
	let call = pallet_test_notifier::Call::notification_received {
		query_id: 0,
		response: Default::default(),
	};
	let notify = RuntimeCall::TestNotifier(call);
	new_test_ext_with_balances(balances).execute_with(|| {
		XcmPallet::report_outcome_notify(
			&mut message,
			Parachain(OTHER_PARA_ID).into_location(),
			notify,
			100,
		)
		.unwrap();
		assert_eq!(
			message,
			Xcm(vec![
				SetAppendix(Xcm(vec![ReportError(QueryResponseInfo {
					destination: Parent.into(),
					query_id: 0,
					max_weight: Weight::from_parts(1_000_000, 1_000_000),
				})])),
				TransferAsset { assets: (Here, SEND_AMOUNT).into(), beneficiary: sender },
			])
		);
		let querier: Location = Here.into();
		let status = QueryStatus::Pending {
			responder: Location::from(Parachain(OTHER_PARA_ID)).into(),
			maybe_notify: Some((5, 2)),
			timeout: 100,
			maybe_match_querier: Some(querier.clone().into()),
		};
		assert_eq!(crate::Queries::<Test>::iter().collect::<Vec<_>>(), vec![(0, status)]);

		let message = Xcm(vec![QueryResponse {
			query_id: 0,
			response: Response::ExecutionResult(None),
			max_weight: Weight::from_parts(1_000_000, 1_000_000),
			querier: Some(querier),
		}]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			Parachain(OTHER_PARA_ID),
			message,
			&mut hash,
			Weight::from_parts(1_000_000_000, 1_000_000_000),
			Weight::zero(),
		);
		assert_eq!(r, Outcome::Complete { used: Weight::from_parts(1_000, 1_000) });
		assert_eq!(
			last_events(2),
			vec![
				RuntimeEvent::TestNotifier(pallet_test_notifier::Event::ResponseReceived(
					Parachain(OTHER_PARA_ID).into(),
					0,
					Response::ExecutionResult(None),
				)),
				RuntimeEvent::XcmPallet(crate::Event::Notified {
					query_id: 0,
					pallet_index: 5,
					call_index: 2
				}),
			]
		);
		assert_eq!(crate::Queries::<Test>::iter().collect::<Vec<_>>(), vec![]);
	});
}

#[test]
fn report_outcome_works() {
	let balances = vec![
		(ALICE, INITIAL_BALANCE),
		(ParaId::from(OTHER_PARA_ID).into_account_truncating(), INITIAL_BALANCE),
	];
	let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
	let mut message = Xcm(vec![TransferAsset {
		assets: (Here, SEND_AMOUNT).into(),
		beneficiary: sender.clone(),
	}]);
	new_test_ext_with_balances(balances).execute_with(|| {
		XcmPallet::report_outcome(&mut message, Parachain(OTHER_PARA_ID).into_location(), 100)
			.unwrap();
		assert_eq!(
			message,
			Xcm(vec![
				SetAppendix(Xcm(vec![ReportError(QueryResponseInfo {
					destination: Parent.into(),
					query_id: 0,
					max_weight: Weight::zero(),
				})])),
				TransferAsset { assets: (Here, SEND_AMOUNT).into(), beneficiary: sender },
			])
		);
		let querier: Location = Here.into();
		let status = QueryStatus::Pending {
			responder: Location::from(Parachain(OTHER_PARA_ID)).into(),
			maybe_notify: None,
			timeout: 100,
			maybe_match_querier: Some(querier.clone().into()),
		};
		assert_eq!(crate::Queries::<Test>::iter().collect::<Vec<_>>(), vec![(0, status)]);

		let message = Xcm(vec![QueryResponse {
			query_id: 0,
			response: Response::ExecutionResult(None),
			max_weight: Weight::zero(),
			querier: Some(querier),
		}]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			Parachain(OTHER_PARA_ID),
			message,
			&mut hash,
			Weight::from_parts(1_000_000_000, 1_000_000_000),
			Weight::zero(),
		);
		assert_eq!(r, Outcome::Complete { used: Weight::from_parts(1_000, 1_000) });
		assert_eq!(
			last_event(),
			RuntimeEvent::XcmPallet(crate::Event::ResponseReady {
				query_id: 0,
				response: Response::ExecutionResult(None),
			})
		);

		let response =
			QueryResponseStatus::Ready { response: Response::ExecutionResult(None), at: 1 };
		assert_eq!(XcmPallet::take_response(0), response);
	});
}

#[test]
fn custom_querier_works() {
	let balances = vec![
		(ALICE, INITIAL_BALANCE),
		(ParaId::from(OTHER_PARA_ID).into_account_truncating(), INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let querier: Location = (Parent, AccountId32 { network: None, id: ALICE.into() }).into();

		let r = TestNotifier::prepare_new_query(RuntimeOrigin::signed(ALICE), querier.clone());
		assert_eq!(r, Ok(()));
		let status = QueryStatus::Pending {
			responder: Location::from(AccountId32 { network: None, id: ALICE.into() }).into(),
			maybe_notify: None,
			timeout: 100,
			maybe_match_querier: Some(querier.clone().into()),
		};
		assert_eq!(crate::Queries::<Test>::iter().collect::<Vec<_>>(), vec![(0, status)]);

		// Supplying no querier when one is expected will fail
		let message = Xcm(vec![QueryResponse {
			query_id: 0,
			response: Response::ExecutionResult(None),
			max_weight: Weight::zero(),
			querier: None,
		}]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			AccountId32 { network: None, id: ALICE.into() },
			message,
			&mut hash,
			Weight::from_parts(1_000_000_000, 1_000_000_000),
			Weight::from_parts(1_000, 1_000),
		);
		assert_eq!(r, Outcome::Complete { used: Weight::from_parts(1_000, 1_000) });
		assert_eq!(
			last_event(),
			RuntimeEvent::XcmPallet(crate::Event::InvalidQuerier {
				origin: AccountId32 { network: None, id: ALICE.into() }.into(),
				query_id: 0,
				expected_querier: querier.clone(),
				maybe_actual_querier: None,
			}),
		);

		// Supplying the wrong querier will also fail
		let message = Xcm(vec![QueryResponse {
			query_id: 0,
			response: Response::ExecutionResult(None),
			max_weight: Weight::zero(),
			querier: Some(Location::here()),
		}]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			AccountId32 { network: None, id: ALICE.into() },
			message,
			&mut hash,
			Weight::from_parts(1_000_000_000, 1_000_000_000),
			Weight::from_parts(1_000, 1_000),
		);
		assert_eq!(r, Outcome::Complete { used: Weight::from_parts(1_000, 1_000) });
		assert_eq!(
			last_event(),
			RuntimeEvent::XcmPallet(crate::Event::InvalidQuerier {
				origin: AccountId32 { network: None, id: ALICE.into() }.into(),
				query_id: 0,
				expected_querier: querier.clone(),
				maybe_actual_querier: Some(Location::here()),
			}),
		);

		// Multiple failures should not have changed the query state
		let message = Xcm(vec![QueryResponse {
			query_id: 0,
			response: Response::ExecutionResult(None),
			max_weight: Weight::zero(),
			querier: Some(querier),
		}]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			AccountId32 { network: None, id: ALICE.into() },
			message,
			&mut hash,
			Weight::from_parts(1_000_000_000, 1_000_000_000),
			Weight::zero(),
		);
		assert_eq!(r, Outcome::Complete { used: Weight::from_parts(1_000, 1_000) });
		assert_eq!(
			last_event(),
			RuntimeEvent::XcmPallet(crate::Event::ResponseReady {
				query_id: 0,
				response: Response::ExecutionResult(None),
			})
		);

		let response =
			QueryResponseStatus::Ready { response: Response::ExecutionResult(None), at: 1 };
		assert_eq!(XcmPallet::take_response(0), response);
	});
}

/// Test sending an `XCM` message (`XCM::ReserveAssetDeposit`)
///
/// Asserts that the expected message is sent and the event is emitted
#[test]
fn send_works() {
	let balances = vec![
		(ALICE, INITIAL_BALANCE),
		(ParaId::from(OTHER_PARA_ID).into_account_truncating(), INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			ClearOrigin,
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender.clone() },
		]);

		let versioned_dest = Box::new(RelayLocation::get().into());
		let versioned_message = Box::new(VersionedXcm::from(message.clone()));
		assert_ok!(XcmPallet::send(
			RuntimeOrigin::signed(ALICE),
			versioned_dest,
			versioned_message
		));
		let sent_message = Xcm(Some(DescendOrigin(sender.clone().try_into().unwrap()))
			.into_iter()
			.chain(message.0.clone().into_iter())
			.collect());
		let id = fake_message_hash(&sent_message);
		assert_eq!(sent_xcm(), vec![(Here.into(), sent_message)]);
		assert_eq!(
			last_event(),
			RuntimeEvent::XcmPallet(crate::Event::Sent {
				origin: sender,
				destination: RelayLocation::get(),
				message,
				message_id: id,
			})
		);
	});
}

/// Test that sending an `XCM` message fails when the `XcmRouter` blocks the
/// matching message format
///
/// Asserts that `send` fails with `Error::SendFailure`
#[test]
fn send_fails_when_xcm_router_blocks() {
	use sp_tracing::{
		test_log_capture::init_log_capture,
		tracing::{subscriber, Level},
	};

	let balances = vec![
		(ALICE, INITIAL_BALANCE),
		(ParaId::from(OTHER_PARA_ID).into_account_truncating(), INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let sender: Location = AccountId32 { network: None, id: ALICE.into() }.into();
		let message = Xcm(vec![
			ReserveAssetDeposited((Parent, SEND_AMOUNT).into()),
			buy_execution((Parent, SEND_AMOUNT)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: sender },
		]);
		let (log_capture, subscriber) = init_log_capture(Level::DEBUG, true);
		subscriber::with_default(subscriber, || {
			let result = XcmPallet::send(
				RuntimeOrigin::signed(ALICE),
				Box::new(Location::ancestor(8).into()),
				Box::new(VersionedXcm::from(message.clone())),
			);
			assert_noop!(result, Error::<Test>::SendFailure);
			assert!(log_capture
				.contains("xcm::pallet_xcm::send: XCM send failed with error error=Transport(\"Destination location full\")"));
		});
	});
}

/// Test local execution of XCM
///
/// Asserts that the sender's balance is decreased and the beneficiary's balance
/// is increased. Verifies the expected event is emitted.
#[test]
fn execute_withdraw_to_deposit_works() {
	let balances = vec![
		(ALICE, INITIAL_BALANCE),
		(ParaId::from(OTHER_PARA_ID).into_account_truncating(), INITIAL_BALANCE),
	];
	new_test_ext_with_balances(balances).execute_with(|| {
		let weight = BaseXcmWeight::get() * 3;
		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);
		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(Xcm(vec![
				WithdrawAsset((Here, SEND_AMOUNT).into()),
				buy_execution((Here, SEND_AMOUNT)),
				DepositAsset { assets: AllCounted(1).into(), beneficiary: dest },
			]))),
			weight
		));
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE - SEND_AMOUNT);
		assert_eq!(Balances::total_balance(&BOB), SEND_AMOUNT);
		assert_eq!(
			last_event(),
			RuntimeEvent::XcmPallet(crate::Event::Attempted {
				outcome: Outcome::Complete { used: weight }
			})
		);
	});
}

/// Defense-in-depth: extrinsic failure leads to a full storage rollback, but even so, a
/// partial-fail multi-asset `WithdrawAsset` must not leak a `Credit` imbalance across the
/// transactional rollback and should preserve the `TotalIssuance == Σ balances` invariant.
///
/// The message withdraws the native asset (succeeds, producing a `NegativeImbalance`) and a
/// foreign asset that `ALICE` does not hold (fails). The whole instruction must roll back,
/// restoring `ALICE`'s balance _and_ leaving `TotalIssuance` untouched.
#[test]
fn partial_fail_withdraw_asset_preserves_total_issuance() {
	let balances = vec![(ALICE, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let weight = BaseXcmWeight::get() * 2;
		let alice_location: Location = AccountId32 { network: None, id: ALICE.into() }.into();

		let total_issuance_before = Balances::total_issuance();
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);

		// Native `Here` asset sorts first and withdraws successfully (producing a live
		// `NegativeImbalance`); the foreign asset (which `ALICE` holds none of) is withdrawn
		// second and fails, aborting the instruction.
		let assets: Assets = vec![Asset::from((Here, SEND_AMOUNT)), ForeignAsset::get()].into();
		let message = Xcm(vec![WithdrawAsset(assets)]);
		let mut hash = fake_message_hash(&message);

		// Drive the executor directly: going through extrinsic masks the issue since extrinsic
		// fully reverts any storage changes.
		let outcome = XcmExecutor::<XcmConfig>::prepare_and_execute(
			alice_location,
			message,
			&mut hash,
			weight,
			weight, // weight credit, so `TakeWeightCredit` lets the message through the barrier
		);
		// The multi-asset withdraw must fail because `ALICE` holds none of the foreign asset.
		assert!(
			matches!(outcome, Outcome::Incomplete { .. }),
			"partial-fail withdraw must abort the instruction, got {:?}",
			outcome
		);

		// Rollback must fully restore state: `ALICE`'s native balance is untouched
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);
		// and total issuance is preserved.
		assert_eq!(
			Balances::total_issuance(),
			total_issuance_before,
			"TotalIssuance must be preserved after a partial-fail WithdrawAsset rollback"
		);
	});
}

/// Test XCM authorized aliases.
#[test]
fn authorized_aliases_work() {
	let balances = vec![(ALICE, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		// --- alias is same as origin
		let alias: Location = AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(
			XcmPallet::add_authorized_alias(
				RuntimeOrigin::signed(BOB),
				Box::new(alias.into()),
				None
			),
			Err(Error::<Test>::BadLocation.into())
		);

		// --- alias already expired
		let alias = Location::here();
		let expires = Some(System::current_block_number().saturated_into::<u64>());
		assert_eq!(
			XcmPallet::add_authorized_alias(
				RuntimeOrigin::signed(BOB),
				Box::new(alias.clone().into()),
				expires
			),
			Err(Error::<Test>::ExpiresInPast.into())
		);

		// --- storage deposit not covered (BOB has no funds)
		assert_eq!(
			XcmPallet::add_authorized_alias(
				RuntimeOrigin::signed(BOB),
				Box::new(alias.clone().into()),
				None
			),
			Err(sp_runtime::DispatchError::Token(TokenError::FundsUnavailable))
		);

		// --- setting single alias works
		let who = ALICE;
		let first_alias: Location = AccountId32 { network: Some(Polkadot), id: BOB.into() }.into();
		let total_balance_before = <Balances as Currency<_>>::total_balance(&who);
		let free_balance = <Balances as Currency<_>>::free_balance(&who);
		assert_eq!(free_balance, total_balance_before);
		assert_ok!(XcmPallet::add_authorized_alias(
			RuntimeOrigin::signed(who.clone()),
			Box::new(first_alias.clone().into()),
			None
		));
		let footprint = aliasers_footprint(1);
		let deposit = footprint.size + 2 * footprint.count;
		let free_balance = <Balances as Currency<_>>::free_balance(&who);
		let total_balance = <Balances as Currency<_>>::total_balance(&who);
		assert_eq!(total_balance, total_balance_before);
		assert_eq!(total_balance, free_balance + deposit as u128);

		// --- setting same alias again only updates its expiry
		assert_ok!(XcmPallet::add_authorized_alias(
			RuntimeOrigin::signed(who.clone()),
			Box::new(first_alias.clone().into()),
			Some(100)
		));
		// deposit is unchanged
		assert_eq!(total_balance - deposit as u128, <Balances as Currency<_>>::free_balance(&who));

		// --- setting max number of aliases works
		let mut aliases = vec![];
		for i in 1..MaxAuthorizedAliases::get() {
			let alias = Location::new(0, [Parachain(OTHER_PARA_ID), GeneralIndex(i as u128)]);
			assert_ok!(XcmPallet::add_authorized_alias(
				RuntimeOrigin::signed(who.clone()),
				Box::new(alias.clone().into()),
				None
			));
			let footprint = aliasers_footprint(i as usize + 1);
			let deposit = (footprint.size + 2 * footprint.count) as u128;
			assert_eq!(total_balance - deposit, <Balances as Currency<_>>::free_balance(&who));
			aliases.push(alias);
		}

		// deposit held for MaxAliases
		let footprint = aliasers_footprint(MaxAuthorizedAliases::get() as usize);
		let deposit = (footprint.size + 2 * footprint.count) as u128;
		assert_eq!(total_balance - deposit, <Balances as Currency<_>>::free_balance(&who));

		// --- adding more than max aliases is not allowed
		let alias = Location::new(
			0,
			[Parachain(OTHER_PARA_ID), GeneralIndex(MaxAuthorizedAliases::get() as u128 + 100)],
		);
		assert_eq!(
			XcmPallet::add_authorized_alias(
				RuntimeOrigin::signed(who.clone()),
				Box::new(alias.clone().into()),
				None
			),
			Err(Error::<Test>::TooManyAuthorizedAliases.into())
		);

		// --- remove one alias
		let target: Location = AccountId32 { network: None, id: who.clone().into() }.into();
		assert_ok!(XcmPallet::remove_authorized_alias(
			RuntimeOrigin::signed(who.clone()),
			Box::new(first_alias.clone().into()),
		));
		// deposit held for MaxAliases - 1
		let footprint = aliasers_footprint(MaxAuthorizedAliases::get() as usize - 1);
		let deposit = (footprint.size + 2 * footprint.count) as u128;
		assert_eq!(total_balance - deposit, <Balances as Currency<_>>::free_balance(&who));
		// de-authorization event
		assert_eq!(
			last_events(1),
			vec![RuntimeEvent::XcmPallet(crate::Event::AliasAuthorizationRemoved {
				aliaser: first_alias.into(),
				target: target.clone().into(),
			})]
		);

		// --- adding one more is now allowed
		assert_ok!(XcmPallet::add_authorized_alias(
			RuntimeOrigin::signed(who.clone()),
			Box::new(alias.clone().into()),
			None
		));
		assert_eq!(
			last_events(1),
			vec![RuntimeEvent::XcmPallet(crate::Event::AliasAuthorized {
				aliaser: alias.clone().into(),
				target: target.clone().into(),
				expiry: None,
			})]
		);

		// --- un-authorized alias is correctly filtered/denied
		assert!(!AuthorizedAliasers::<Test>::contains(&Location::here(), &target));
		// --- authorized aliases are correctly allowed
		for i in 1..MaxAuthorizedAliases::get() {
			assert!(AuthorizedAliasers::<Test>::contains(&aliases[i as usize - 1], &target));
		}
		// --- remove alias then verify no longer allowed
		assert_ok!(XcmPallet::remove_authorized_alias(
			RuntimeOrigin::signed(who.clone()),
			Box::new(aliases[0].clone().into()),
		));
		assert!(!AuthorizedAliasers::<Test>::contains(&aliases[0], &target));

		// --- remove nonexistent alias
		assert_eq!(
			XcmPallet::remove_authorized_alias(
				RuntimeOrigin::signed(ALICE),
				Box::new(Location::parent().into())
			),
			Err(Error::<Test>::AliasNotFound.into())
		);

		// --- remove nonexistent alias (BOB has no registered aliases)
		assert_eq!(
			XcmPallet::remove_authorized_alias(
				RuntimeOrigin::signed(BOB),
				Box::new(Location::parent().into()),
			),
			Err(Error::<Test>::AliasNotFound.into())
		);

		// --- remove all aliases then verify all deposit is returned
		assert_ok!(XcmPallet::remove_all_authorized_aliases(RuntimeOrigin::signed(who.clone())));
		// de-authorization event
		assert_eq!(
			last_events(1),
			vec![RuntimeEvent::XcmPallet(crate::Event::AliasesAuthorizationsRemoved {
				target: target.clone().into(),
			})]
		);
		// all deposit is returned
		assert_eq!(total_balance_before, <Balances as Currency<_>>::free_balance(&who));
	});
}

/// Test drop/claim assets.
#[test]
fn trapped_assets_can_be_claimed() {
	let balances = vec![(ALICE, INITIAL_BALANCE), (BOB, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let weight = BaseXcmWeight::get() * 6;
		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();

		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(Xcm(vec![
				WithdrawAsset((Here, SEND_AMOUNT).into()),
				buy_execution((Here, SEND_AMOUNT)),
				// Don't propagated the error into the result.
				SetErrorHandler(Xcm(vec![ClearError])),
				// This will make an error.
				Trap(0),
				// This would succeed, but we never get to it.
				DepositAsset { assets: AllCounted(1).into(), beneficiary: dest.clone() },
			]))),
			weight
		));
		let source: Location = Junction::AccountId32 { network: None, id: ALICE.into() }.into();
		let trapped = AssetTraps::<Test>::iter().collect::<Vec<_>>();
		let vma = VersionedAssets::from(Assets::from((Here, SEND_AMOUNT)));
		let hash = BlakeTwo256::hash_of(&(source.clone(), vma.clone()));
		assert_eq!(
			last_events(2),
			vec![
				RuntimeEvent::XcmPallet(crate::Event::AssetsTrapped {
					hash,
					origin: source,
					assets: vma
				}),
				RuntimeEvent::XcmPallet(crate::Event::Attempted {
					outcome: Outcome::Complete { used: BaseXcmWeight::get() * 5 }
				}),
			]
		);
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE - SEND_AMOUNT);
		assert_eq!(Balances::total_balance(&BOB), INITIAL_BALANCE);

		let expected = vec![(hash, 1u32)];
		assert_eq!(trapped, expected);

		let weight = BaseXcmWeight::get() * 3;
		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(Xcm(vec![
				ClaimAsset { assets: (Here, SEND_AMOUNT).into(), ticket: Here.into() },
				buy_execution((Here, SEND_AMOUNT)),
				DepositAsset { assets: AllCounted(1).into(), beneficiary: dest.clone() },
			]))),
			weight
		));

		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE - SEND_AMOUNT);
		assert_eq!(Balances::total_balance(&BOB), INITIAL_BALANCE + SEND_AMOUNT);
		assert_eq!(AssetTraps::<Test>::iter().collect::<Vec<_>>(), vec![]);

		// Can't claim twice.
		assert_err_ignore_postinfo!(
			XcmPallet::execute(
				RuntimeOrigin::signed(ALICE),
				Box::new(VersionedXcm::from(Xcm(vec![
					ClaimAsset { assets: (Here, SEND_AMOUNT).into(), ticket: Here.into() },
					buy_execution((Here, SEND_AMOUNT)),
					DepositAsset { assets: AllCounted(1).into(), beneficiary: dest },
				]))),
				weight
			),
			Error::<Test>::LocalExecutionIncompleteWithError {
				index: 0,
				error: XcmError::UnknownClaim.into()
			}
		);
	});
}

// Like `trapped_assets_can_be_claimed` but using the `claim_assets` extrinsic.
#[test]
fn claim_assets_works() {
	let balances = vec![(ALICE, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		// First trap some assets.
		let trapping_program =
			Xcm::<RuntimeCall>::builder_unsafe().withdraw_asset((Here, SEND_AMOUNT)).build();
		// Even though assets are trapped, the extrinsic returns success.
		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(trapping_program)),
			BaseXcmWeight::get() * 2,
		));
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE - SEND_AMOUNT);

		// Expected `AssetsTrapped` event info.
		let source: Location = Junction::AccountId32 { network: None, id: ALICE.into() }.into();
		let versioned_assets = VersionedAssets::from(Assets::from((Here, SEND_AMOUNT)));
		let hash = BlakeTwo256::hash_of(&(source.clone(), versioned_assets.clone()));

		// Assets were indeed trapped.
		assert_eq!(
			last_events(2),
			vec![
				RuntimeEvent::XcmPallet(crate::Event::AssetsTrapped {
					hash,
					origin: source,
					assets: versioned_assets
				}),
				RuntimeEvent::XcmPallet(crate::Event::Attempted {
					outcome: Outcome::Complete { used: BaseXcmWeight::get() * 1 }
				})
			],
		);
		let trapped = AssetTraps::<Test>::iter().collect::<Vec<_>>();
		assert_eq!(trapped, vec![(hash, 1)]);

		// Now claim them with the extrinsic.
		assert_ok!(XcmPallet::claim_assets(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedAssets::from(Assets::from((Here, SEND_AMOUNT)))),
			Box::new(VersionedLocation::from(Location::from(AccountId32 {
				network: None,
				id: ALICE.clone().into()
			}))),
		));
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);
		assert_eq!(AssetTraps::<Test>::iter().collect::<Vec<_>>(), vec![]);
	});
}

// Like `claim_assets_works` but claiming a trap holding more than one asset.
#[test]
fn claim_assets_works_with_multiple_assets() {
	let balances = vec![(ALICE, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		use crate::tests::assets_transfer::set_up_foreign_asset;
		let foreign_amount = 100u128;
		let (_, _, foreign_asset_id_location) = set_up_foreign_asset(
			FOREIGN_ASSET_RESERVE_PARA_ID,
			Some(FOREIGN_ASSET_INNER_JUNCTION),
			ALICE,
			foreign_amount,
			true,
		);
		let assets: Assets = vec![
			(Here, SEND_AMOUNT).into(),
			(foreign_asset_id_location.clone(), foreign_amount).into(),
		]
		.into();

		// First trap both assets with a single trap.
		let trapping_program =
			Xcm::<RuntimeCall>::builder_unsafe().withdraw_asset(assets.clone()).build();
		// Even though assets are trapped, the extrinsic returns success.
		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(trapping_program)),
			BaseXcmWeight::get() * 2,
		));
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE - SEND_AMOUNT);
		assert_eq!(AssetsPallet::balance(foreign_asset_id_location.clone(), &ALICE), 0);
		assert_eq!(AssetTraps::<Test>::iter().count(), 1);

		// Now claim them all back with the extrinsic.
		assert_ok!(XcmPallet::claim_assets(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedAssets::from(assets)),
			Box::new(VersionedLocation::from(Location::from(AccountId32 {
				network: None,
				id: ALICE.clone().into()
			}))),
		));
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);
		assert_eq!(AssetsPallet::balance(foreign_asset_id_location, &ALICE), foreign_amount);
		assert_eq!(AssetTraps::<Test>::iter().count(), 0);
	});
}

/// `claim_assets` weight must grow with the number of claimed assets, otherwise a
/// many-asset claim is underweighted and can be used to build overweight blocks.
#[test]
fn claim_assets_weight_scales_with_number_of_assets() {
	use frame_support::dispatch::GetDispatchInfo;
	let beneficiary = Box::new(VersionedLocation::from(Location::from(AccountId32 {
		network: None,
		id: ALICE.clone().into(),
	})));
	let one_asset: Assets = (Here, SEND_AMOUNT).into();
	let many_assets: Assets = (0..MAX_ITEMS_IN_ASSETS as u128)
		.map(|i| (Location::new(0, [GeneralIndex(i)]), SEND_AMOUNT).into())
		.collect::<Vec<Asset>>()
		.into();
	let claim_one = crate::Call::<Test>::claim_assets {
		assets: Box::new(VersionedAssets::from(one_asset)),
		beneficiary: beneficiary.clone(),
	};
	let claim_many = crate::Call::<Test>::claim_assets {
		assets: Box::new(VersionedAssets::from(many_assets)),
		beneficiary,
	};
	assert!(
		claim_many.get_dispatch_info().call_weight.ref_time() >
			claim_one.get_dispatch_info().call_weight.ref_time()
	);
}

/// Test failure to complete execution reverts intermediate side-effects.
///
/// XCM program will withdraw and deposit some assets, then fail execution of a further withdraw.
/// Assert that the previous instructions effects are reverted.
#[test]
fn incomplete_execute_reverts_side_effects() {
	let balances = vec![(ALICE, INITIAL_BALANCE), (BOB, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let weight = BaseXcmWeight::get() * 4;
		let dest: Location = Junction::AccountId32 { network: None, id: BOB.into() }.into();
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);
		let amount_to_send = INITIAL_BALANCE - ExistentialDeposit::get();
		let assets: Assets = (Here, amount_to_send).into();
		let result = XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(Xcm(vec![
				// Withdraw + BuyExec + Deposit should work
				WithdrawAsset(assets.clone()),
				buy_execution(assets.inner()[0].clone()),
				DepositAsset { assets: assets.clone().into(), beneficiary: dest },
				// Withdrawing once more will fail because of InsufficientBalance, and we expect to
				// revert the effects of the above instructions as well
				WithdrawAsset(assets),
			]))),
			weight,
		);
		// all effects are reverted and balances unchanged for either sender or receiver
		assert_eq!(Balances::total_balance(&ALICE), INITIAL_BALANCE);
		assert_eq!(Balances::total_balance(&BOB), INITIAL_BALANCE);

		assert_eq!(
			result,
			Err(sp_runtime::DispatchErrorWithPostInfo {
				post_info: frame_support::dispatch::PostDispatchInfo {
					actual_weight: Some(
						<Pallet<Test> as ExecuteControllerWeightInfo>::execute() + weight
					),
					pays_fee: frame_support::dispatch::Pays::Yes,
				},
				error: sp_runtime::DispatchError::from(
					Error::<Test>::LocalExecutionIncompleteWithError {
						index: 3,
						error: XcmError::FailedToTransactAsset("").into()
					}
				)
			})
		);
	});
}

#[test]
fn fake_latest_versioned_location_works() {
	use codec::Encode;
	let remote: Location = Parachain(1000).into();
	let versioned_remote = LatestVersionedLocation(&remote);
	assert_eq!(versioned_remote.encode(), remote.into_versioned().encode());
}

#[test]
fn basic_subscription_works() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		let remote: Location = Parachain(1000).into();
		assert_ok!(XcmPallet::force_subscribe_version_notify(
			RuntimeOrigin::root(),
			Box::new(remote.clone().into()),
		));

		assert_eq!(
			Queries::<Test>::iter().collect::<Vec<_>>(),
			vec![(
				0,
				QueryStatus::VersionNotifier { origin: remote.clone().into(), is_active: false }
			)]
		);
		assert_eq!(
			VersionNotifiers::<Test>::iter().collect::<Vec<_>>(),
			vec![(XCM_VERSION, remote.clone().into(), 0)]
		);

		assert_eq!(
			take_sent_xcm(),
			vec![(
				remote.clone(),
				Xcm(vec![SubscribeVersion { query_id: 0, max_response_weight: Weight::zero() }]),
			),]
		);

		let weight = BaseXcmWeight::get();
		let mut message = Xcm::<()>(vec![
			// Remote supports XCM v3
			QueryResponse {
				query_id: 0,
				max_weight: Weight::zero(),
				response: Response::Version(3),
				querier: None,
			},
		]);
		assert_ok!(AllowKnownQueryResponses::<XcmPallet>::should_execute(
			&remote,
			message.inner_mut(),
			weight,
			&mut Properties { weight_credit: Weight::zero(), message_id: None },
		));
	});
}

#[test]
fn subscriptions_increment_id() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		let remote: Location = Parachain(1000).into();
		assert_ok!(XcmPallet::force_subscribe_version_notify(
			RuntimeOrigin::root(),
			Box::new(remote.clone().into()),
		));

		let remote2: Location = Parachain(1001).into();
		assert_ok!(XcmPallet::force_subscribe_version_notify(
			RuntimeOrigin::root(),
			Box::new(remote2.clone().into()),
		));

		assert_eq!(
			take_sent_xcm(),
			vec![
				(
					remote,
					Xcm(vec![SubscribeVersion {
						query_id: 0,
						max_response_weight: Weight::zero()
					}]),
				),
				(
					remote2,
					Xcm(vec![SubscribeVersion {
						query_id: 1,
						max_response_weight: Weight::zero()
					}]),
				),
			]
		);
	});
}

#[test]
fn double_subscription_fails() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		let remote: Location = Parachain(1000).into();
		assert_ok!(XcmPallet::force_subscribe_version_notify(
			RuntimeOrigin::root(),
			Box::new(remote.clone().into()),
		));
		assert_noop!(
			XcmPallet::force_subscribe_version_notify(
				RuntimeOrigin::root(),
				Box::new(remote.into())
			),
			Error::<Test>::AlreadySubscribed,
		);
	})
}

#[test]
fn unsubscribe_works() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		let remote: Location = Parachain(1000).into();
		assert_ok!(XcmPallet::force_subscribe_version_notify(
			RuntimeOrigin::root(),
			Box::new(remote.clone().into()),
		));
		assert_ok!(XcmPallet::force_unsubscribe_version_notify(
			RuntimeOrigin::root(),
			Box::new(remote.clone().into())
		));
		assert_noop!(
			XcmPallet::force_unsubscribe_version_notify(
				RuntimeOrigin::root(),
				Box::new(remote.clone().into())
			),
			Error::<Test>::NoSubscription,
		);

		assert_eq!(
			take_sent_xcm(),
			vec![
				(
					remote.clone(),
					Xcm(vec![SubscribeVersion {
						query_id: 0,
						max_response_weight: Weight::zero()
					}]),
				),
				(remote.clone(), Xcm(vec![UnsubscribeVersion]),),
			]
		);
	});
}

/// Parachain 1000 is asking us for a version subscription.
#[test]
fn subscription_side_works() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		AdvertisedXcmVersion::set(1);

		let remote: Location = Parachain(1000).into();
		let weight = BaseXcmWeight::get();
		let message =
			Xcm(vec![SubscribeVersion { query_id: 0, max_response_weight: Weight::zero() }]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			remote.clone(),
			message,
			&mut hash,
			weight,
			Weight::zero(),
		);
		assert_eq!(r, Outcome::Complete { used: weight });

		let instr = QueryResponse {
			query_id: 0,
			max_weight: Weight::zero(),
			response: Response::Version(1),
			querier: None,
		};
		assert_eq!(take_sent_xcm(), vec![(remote.clone(), Xcm(vec![instr]))]);

		// A runtime upgrade which doesn't alter the version sends no notifications.
		CurrentMigration::<Test>::put(VersionMigrationStage::default());
		XcmPallet::on_initialize(1);
		assert_eq!(take_sent_xcm(), vec![]);

		// New version.
		AdvertisedXcmVersion::set(2);

		// A runtime upgrade which alters the version does send notifications.
		CurrentMigration::<Test>::put(VersionMigrationStage::default());
		XcmPallet::on_initialize(2);
		let instr = QueryResponse {
			query_id: 0,
			max_weight: Weight::zero(),
			response: Response::Version(2),
			querier: None,
		};
		assert_eq!(take_sent_xcm(), vec![(remote, Xcm(vec![instr]))]);
	});
}

#[test]
fn subscription_side_upgrades_work_with_notify() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		AdvertisedXcmVersion::set(1);

		// An entry from a previous runtime with v3 XCM.
		let v3_location = VersionedLocation::V3(xcm::v3::Junction::Parachain(1001).into());
		VersionNotifyTargets::<Test>::insert(3, v3_location, (70, Weight::zero(), 3));
		let v4_location = Parachain(1003).into_versioned();
		VersionNotifyTargets::<Test>::insert(4, v4_location, (72, Weight::zero(), 3));

		// New version.
		AdvertisedXcmVersion::set(4);

		// A runtime upgrade which alters the version does send notifications.
		CurrentMigration::<Test>::put(VersionMigrationStage::default());
		XcmPallet::on_initialize(1);

		let instr1 = QueryResponse {
			query_id: 70,
			max_weight: Weight::zero(),
			response: Response::Version(4),
			querier: None,
		};
		let instr3 = QueryResponse {
			query_id: 72,
			max_weight: Weight::zero(),
			response: Response::Version(4),
			querier: None,
		};
		let mut sent = take_sent_xcm();
		sent.sort_by_key(|k| match (k.1).0[0] {
			QueryResponse { query_id: q, .. } => q,
			_ => 0,
		});
		assert_eq!(
			sent,
			vec![
				(Parachain(1001).into(), Xcm(vec![instr1])),
				(Parachain(1003).into(), Xcm(vec![instr3])),
			]
		);

		let mut contents = VersionNotifyTargets::<Test>::iter().collect::<Vec<_>>();
		contents.sort_by_key(|k| k.2 .0);
		assert_eq!(
			contents,
			vec![
				(XCM_VERSION, Parachain(1001).into_versioned(), (70, Weight::zero(), 4)),
				(XCM_VERSION, Parachain(1003).into_versioned(), (72, Weight::zero(), 4)),
			]
		);
	});
}

#[test]
fn subscription_side_upgrades_work_without_notify() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		// An entry from a previous runtime with v3 XCM.
		let v3_location = VersionedLocation::V3(xcm::v3::Junction::Parachain(1001).into());
		VersionNotifyTargets::<Test>::insert(3, v3_location, (70, Weight::zero(), 3));
		let v4_location = Parachain(1003).into_versioned();
		VersionNotifyTargets::<Test>::insert(4, v4_location, (72, Weight::zero(), 3));

		// A runtime upgrade which alters the version does send notifications.
		CurrentMigration::<Test>::put(VersionMigrationStage::default());
		XcmPallet::on_initialize(1);

		let mut contents = VersionNotifyTargets::<Test>::iter().collect::<Vec<_>>();
		contents.sort_by_key(|k| k.2 .0);
		assert_eq!(
			contents,
			vec![
				(XCM_VERSION, Parachain(1001).into_versioned(), (70, Weight::zero(), 4)),
				(XCM_VERSION, Parachain(1003).into_versioned(), (72, Weight::zero(), 4)),
			]
		);
	});
}

#[test]
fn subscriber_side_subscription_works() {
	new_test_ext_with_balances_and_xcm_version(vec![], Some(XCM_VERSION), vec![]).execute_with(
		|| {
			let remote: Location = Parachain(1000).into();
			assert_ok!(XcmPallet::force_subscribe_version_notify(
				RuntimeOrigin::root(),
				Box::new(remote.clone().into()),
			));
			assert_eq!(XcmPallet::get_version_for(&remote), None);
			take_sent_xcm();

			// Assume subscription target is working ok.

			let weight = BaseXcmWeight::get();
			let message = Xcm(vec![
				// Remote supports XCM v3
				QueryResponse {
					query_id: 0,
					max_weight: Weight::zero(),
					response: Response::Version(3),
					querier: None,
				},
			]);
			let mut hash = fake_message_hash(&message);
			let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
				remote.clone(),
				message,
				&mut hash,
				weight,
				Weight::zero(),
			);
			assert_eq!(r, Outcome::Complete { used: weight });
			assert_eq!(take_sent_xcm(), vec![]);
			assert_eq!(XcmPallet::get_version_for(&remote), Some(3));

			// This message will be sent as v3.
			let v4_msg = xcm::v4::Xcm::<()>(vec![xcm::v4::Instruction::Trap(0)]);
			assert_eq!(
				XcmPallet::wrap_version(&remote, v4_msg.clone()),
				Ok(VersionedXcm::V3(xcm::v3::Xcm(vec![xcm::v3::Instruction::Trap(0)])))
			);

			let message = Xcm(vec![
				// Remote upgraded to XCM v4
				QueryResponse {
					query_id: 0,
					max_weight: Weight::zero(),
					response: Response::Version(4),
					querier: None,
				},
			]);
			let mut hash = fake_message_hash(&message);
			let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
				remote.clone(),
				message,
				&mut hash,
				weight,
				Weight::zero(),
			);
			assert_eq!(r, Outcome::Complete { used: weight });
			assert_eq!(take_sent_xcm(), vec![]);
			assert_eq!(XcmPallet::get_version_for(&remote), Some(4));

			// This message is now sent as v4.
			assert_eq!(
				XcmPallet::wrap_version(&remote, v4_msg.clone()),
				Ok(VersionedXcm::from(v4_msg))
			);
		},
	);
}

/// We should auto-subscribe when we don't know the remote's version.
#[test]
fn auto_subscription_works() {
	new_test_ext_with_balances_and_xcm_version(vec![], None, vec![]).execute_with(|| {
		let remote_v3: Location = Parachain(1000).into();
		let remote_v4: Location = Parachain(1001).into();

		assert_ok!(XcmPallet::force_default_xcm_version(RuntimeOrigin::root(), Some(3)));

		// Wrapping a version for a destination we don't know elicits a subscription.
		let msg_v3 = xcm::v3::Xcm::<()>(vec![xcm::v3::Instruction::Trap(0)]);
		let msg_v4 = xcm::v4::Xcm::<()>(vec![xcm::v4::Instruction::ClearTopic]);
		assert_eq!(
			XcmPallet::wrap_version(&remote_v3, msg_v3.clone()),
			Ok(VersionedXcm::from(msg_v3.clone())),
		);
		assert_eq!(
			XcmPallet::wrap_version(&remote_v3, msg_v4.clone()),
			Ok(VersionedXcm::V3(xcm::v3::Xcm(vec![xcm::v3::Instruction::ClearTopic])))
		);

		let expected = vec![(remote_v3.clone().into(), 2)];
		assert_eq!(VersionDiscoveryQueue::<Test>::get().into_inner(), expected);

		assert_eq!(
			XcmPallet::wrap_version(&remote_v4, msg_v3.clone()),
			Ok(VersionedXcm::from(msg_v3.clone())),
		);
		assert_eq!(
			XcmPallet::wrap_version(&remote_v4, msg_v4.clone()),
			Ok(VersionedXcm::V3(xcm::v3::Xcm(vec![xcm::v3::Instruction::ClearTopic])))
		);

		let expected = vec![(remote_v3.clone().into(), 2), (remote_v4.clone().into(), 2)];
		assert_eq!(VersionDiscoveryQueue::<Test>::get().into_inner(), expected);

		XcmPallet::on_initialize(1);
		assert_eq!(
			take_sent_xcm(),
			vec![(
				remote_v4.clone(),
				Xcm(vec![SubscribeVersion { query_id: 0, max_response_weight: Weight::zero() }]),
			)]
		);

		// Assume remote_v4 is working ok and XCM version 4.

		let weight = BaseXcmWeight::get();
		let message = Xcm(vec![
			// Remote supports XCM v4
			QueryResponse {
				query_id: 0,
				max_weight: Weight::zero(),
				response: Response::Version(4),
				querier: None,
			},
		]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			remote_v4.clone(),
			message,
			&mut hash,
			weight,
			Weight::zero(),
		);
		assert_eq!(r, Outcome::Complete { used: weight });

		// V3 messages can be sent to remote_v4 under XCM v4.
		assert_eq!(
			XcmPallet::wrap_version(&remote_v4, msg_v3.clone()),
			Ok(VersionedXcm::from(msg_v3.clone()).into_version(4).unwrap()),
		);
		// This message can now be sent to remote_v4 as it's v4.
		assert_eq!(
			XcmPallet::wrap_version(&remote_v4, msg_v4.clone()),
			Ok(VersionedXcm::from(msg_v4.clone()))
		);

		XcmPallet::on_initialize(2);
		assert_eq!(
			take_sent_xcm(),
			vec![(
				remote_v3.clone(),
				Xcm(vec![SubscribeVersion { query_id: 1, max_response_weight: Weight::zero() }]),
			)]
		);

		// Assume remote_v3 is working ok and XCM version 3.

		let weight = BaseXcmWeight::get();
		let message = Xcm(vec![
			// Remote supports XCM v3
			QueryResponse {
				query_id: 1,
				max_weight: Weight::zero(),
				response: Response::Version(3),
				querier: None,
			},
		]);
		let mut hash = fake_message_hash(&message);
		let r = XcmExecutor::<XcmConfig>::prepare_and_execute(
			remote_v3.clone(),
			message,
			&mut hash,
			weight,
			Weight::zero(),
		);
		assert_eq!(r, Outcome::Complete { used: weight });

		// v4 messages cannot be sent to remote_v3...
		assert_eq!(
			XcmPallet::wrap_version(&remote_v3, msg_v3.clone()),
			Ok(VersionedXcm::V3(msg_v3))
		);
		assert_eq!(
			XcmPallet::wrap_version(&remote_v3, msg_v4.clone()),
			Ok(VersionedXcm::V3(xcm::v3::Xcm(vec![xcm::v3::Instruction::ClearTopic])))
		);
	})
}

#[test]
fn subscription_side_upgrades_work_with_multistage_notify() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		AdvertisedXcmVersion::set(1);

		// An entry from a previous runtime with v0 XCM.
		let v3_location = VersionedLocation::V3(xcm::v3::Junction::Parachain(1001).into());
		VersionNotifyTargets::<Test>::insert(3, v3_location, (70, Weight::zero(), 3));
		let v3_location = VersionedLocation::V3(xcm::v3::Junction::Parachain(1002).into());
		VersionNotifyTargets::<Test>::insert(3, v3_location, (71, Weight::zero(), 3));
		let v4_location = Parachain(1003).into_versioned();
		VersionNotifyTargets::<Test>::insert(4, v4_location, (72, Weight::zero(), 3));

		// New version.
		AdvertisedXcmVersion::set(4);

		// A runtime upgrade which alters the version does send notifications.
		CurrentMigration::<Test>::put(VersionMigrationStage::default());
		let mut maybe_migration = CurrentMigration::<Test>::take();
		let mut counter = 0;
		while let Some(migration) = maybe_migration.take() {
			counter += 1;
			let (_, m) = XcmPallet::lazy_migration(migration, Weight::zero());
			maybe_migration = m;
		}
		assert_eq!(counter, 4);

		let instr1 = QueryResponse {
			query_id: 70,
			max_weight: Weight::zero(),
			response: Response::Version(4),
			querier: None,
		};
		let instr2 = QueryResponse {
			query_id: 71,
			max_weight: Weight::zero(),
			response: Response::Version(4),
			querier: None,
		};
		let instr3 = QueryResponse {
			query_id: 72,
			max_weight: Weight::zero(),
			response: Response::Version(4),
			querier: None,
		};
		let mut sent = take_sent_xcm();
		sent.sort_by_key(|k| match (k.1).0[0] {
			QueryResponse { query_id: q, .. } => q,
			_ => 0,
		});
		assert_eq!(
			sent,
			vec![
				(Parachain(1001).into(), Xcm(vec![instr1])),
				(Parachain(1002).into(), Xcm(vec![instr2])),
				(Parachain(1003).into(), Xcm(vec![instr3])),
			]
		);

		let mut contents = VersionNotifyTargets::<Test>::iter().collect::<Vec<_>>();
		contents.sort_by_key(|k| k.2 .0);
		assert_eq!(
			contents,
			vec![
				(XCM_VERSION, Parachain(1001).into_versioned(), (70, Weight::zero(), 4)),
				(XCM_VERSION, Parachain(1002).into_versioned(), (71, Weight::zero(), 4)),
				(XCM_VERSION, Parachain(1003).into_versioned(), (72, Weight::zero(), 4)),
			]
		);
	});
}

#[test]
fn get_and_wrap_version_works() {
	let remote_a: Location = Parachain(1000).into();
	let remote_b: Location = Parachain(1001).into();
	let remote_c: Location = Parachain(1002).into();
	let remote_d: Location = Parachain(1003).into();

	new_test_ext_with_balances_and_xcm_version(vec![], None, vec![(remote_d.clone(), XCM_VERSION)])
		.execute_with(|| {
			// Versioned XCM location for `remote_d` was set in the genesis state
			assert_eq!(XcmPallet::get_version_for(&remote_d), Some(XCM_VERSION));

			// no `safe_xcm_version` version at `GenesisConfig`
			assert_eq!(XcmPallet::get_version_for(&remote_a), None);
			assert_eq!(XcmPallet::get_version_for(&remote_b), None);
			assert_eq!(XcmPallet::get_version_for(&remote_c), None);
			assert_eq!(XcmPallet::get_version_for(&remote_d), Some(XCM_VERSION));
			assert_eq!(VersionDiscoveryQueue::<Test>::get().into_inner(), vec![]);

			// set default XCM version (a.k.a. `safe_xcm_version`)
			assert_ok!(XcmPallet::force_default_xcm_version(RuntimeOrigin::root(), Some(1)));
			assert_eq!(XcmPallet::get_version_for(&remote_a), None);
			assert_eq!(XcmPallet::get_version_for(&remote_b), None);
			assert_eq!(XcmPallet::get_version_for(&remote_c), None);
			assert_eq!(XcmPallet::get_version_for(&remote_d), Some(XCM_VERSION));
			assert_eq!(VersionDiscoveryQueue::<Test>::get().into_inner(), vec![]);

			// set XCM version only for `remote_a`
			assert_ok!(XcmPallet::force_xcm_version(
				RuntimeOrigin::root(),
				Box::new(remote_a.clone()),
				XCM_VERSION
			));
			assert_eq!(XcmPallet::get_version_for(&remote_a), Some(XCM_VERSION));
			assert_eq!(XcmPallet::get_version_for(&remote_b), None);
			assert_eq!(XcmPallet::get_version_for(&remote_c), None);
			assert_eq!(XcmPallet::get_version_for(&remote_d), Some(XCM_VERSION));
			assert_eq!(VersionDiscoveryQueue::<Test>::get().into_inner(), vec![]);

			let xcm = Xcm::<()>::default();

			// wrap version - works because remote_a has `XCM_VERSION`
			assert_eq!(
				XcmPallet::wrap_version(&remote_a, xcm.clone()),
				Ok(VersionedXcm::from(xcm.clone()))
			);
			// does not work because remote_b has unknown version and default is set to 1, and
			// `XCM_VERSION` cannot be wrapped to the `1`
			assert_eq!(XcmPallet::wrap_version(&remote_b, xcm.clone()), Err(()));
			assert_eq!(
				VersionDiscoveryQueue::<Test>::get().into_inner(),
				vec![(remote_b.clone().into(), 1)]
			);

			// set default to the `XCM_VERSION`
			assert_ok!(XcmPallet::force_default_xcm_version(
				RuntimeOrigin::root(),
				Some(XCM_VERSION)
			));
			assert_eq!(XcmPallet::get_version_for(&remote_b), None);
			assert_eq!(XcmPallet::get_version_for(&remote_c), None);
			assert_eq!(XcmPallet::get_version_for(&remote_d), Some(XCM_VERSION));

			// now works, because default is `XCM_VERSION`
			assert_eq!(
				XcmPallet::wrap_version(&remote_b, xcm.clone()),
				Ok(VersionedXcm::from(xcm.clone()))
			);
			assert_eq!(
				VersionDiscoveryQueue::<Test>::get().into_inner(),
				vec![(remote_b.clone().into(), 2)]
			);

			// change remote_c to `1`
			assert_ok!(XcmPallet::force_xcm_version(
				RuntimeOrigin::root(),
				Box::new(remote_c.clone()),
				1
			));

			// does not work because remote_c has `1` and default is `XCM_VERSION` which cannot be
			// wrapped to the `1`
			assert_eq!(XcmPallet::wrap_version(&remote_c, xcm.clone()), Err(()));
			assert_eq!(
				VersionDiscoveryQueue::<Test>::get().into_inner(),
				vec![(remote_b.into(), 2)]
			);
		})
}

#[test]
fn multistage_migration_works() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		// An entry from a previous runtime with v3 XCM.
		let v3_location = VersionedLocation::V3(xcm::v3::Junction::Parachain(1001).into());
		let v3_version = xcm::v3::VERSION;
		SupportedVersion::<Test>::insert(v3_version, v3_location.clone(), v3_version);
		VersionNotifiers::<Test>::insert(v3_version, v3_location.clone(), 1);
		VersionNotifyTargets::<Test>::insert(
			v3_version,
			v3_location,
			(70, Weight::zero(), v3_version),
		);
		// A version to advertise.
		AdvertisedXcmVersion::set(4);

		// check `try-state`
		assert!(Pallet::<Test>::do_try_state().is_err());

		// closure simulates a multistage migration process
		let migrate = |expected_cycle_count| {
			// A runtime upgrade which alters the version does send notifications.
			CurrentMigration::<Test>::put(VersionMigrationStage::default());
			let mut maybe_migration = CurrentMigration::<Test>::take();
			let mut counter = 0;
			let mut weight_used = Weight::zero();
			while let Some(migration) = maybe_migration.take() {
				counter += 1;
				let (w, m) = XcmPallet::lazy_migration(migration, Weight::zero());
				maybe_migration = m;
				weight_used.saturating_accrue(w);
			}
			assert_eq!(counter, expected_cycle_count);
			weight_used
		};

		// run migration for the first time
		let _ = migrate(4);

		// check xcm sent
		assert_eq!(
			take_sent_xcm(),
			vec![(
				Parachain(1001).into(),
				Xcm(vec![QueryResponse {
					query_id: 70,
					max_weight: Weight::zero(),
					response: Response::Version(AdvertisedXcmVersion::get()),
					querier: None,
				}])
			),]
		);

		// check migrated data
		assert_eq!(
			SupportedVersion::<Test>::iter().collect::<Vec<_>>(),
			vec![(XCM_VERSION, Parachain(1001).into_versioned(), v3_version),]
		);
		assert_eq!(
			VersionNotifiers::<Test>::iter().collect::<Vec<_>>(),
			vec![(XCM_VERSION, Parachain(1001).into_versioned(), 1),]
		);
		assert_eq!(
			VersionNotifyTargets::<Test>::iter().collect::<Vec<_>>(),
			vec![(XCM_VERSION, Parachain(1001).into_versioned(), (70, Weight::zero(), 4)),]
		);

		// run migration again to check it can run multiple time without any harm or double sending
		// messages.
		let weight_used = migrate(1);
		assert_eq!(weight_used, 1_u8 * <Test as Config>::WeightInfo::already_notified_target());

		// check no xcm sent
		assert_eq!(take_sent_xcm(), vec![]);

		// check `try-state`
		assert!(Pallet::<Test>::do_try_state().is_ok());
	})
}

#[test]
fn migrate_data_to_xcm_version_works() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		// check `try-state`
		assert!(Pallet::<Test>::do_try_state().is_ok());

		let latest_version = XCM_VERSION;
		let previous_version = XCM_VERSION - 1;

		// `Queries` migration
		{
			let origin = VersionedLocation::from(Location::parent());
			let query_id1 = 0;
			let query_id2 = 2;
			let query_as_latest =
				QueryStatus::VersionNotifier { origin: origin.clone(), is_active: true };
			let query_as_previous = QueryStatus::VersionNotifier {
				origin: origin.into_version(previous_version).unwrap(),
				is_active: true,
			};
			assert_ne!(query_as_latest, query_as_previous);
			assert!(!query_as_latest.needs_migration(latest_version));
			assert!(!query_as_latest.needs_migration(previous_version));
			assert!(query_as_previous.needs_migration(latest_version));
			assert!(!query_as_previous.needs_migration(previous_version));

			// store two queries: migrated and not migrated
			Queries::<Test>::insert(query_id1, query_as_latest.clone());
			Queries::<Test>::insert(query_id2, query_as_previous);
			assert!(Pallet::<Test>::do_try_state().is_ok());

			// trigger migration
			Pallet::<Test>::migrate_data_to_xcm_version(&mut Weight::zero(), latest_version);

			// no change for query_id1
			assert_eq!(Queries::<Test>::get(query_id1), Some(query_as_latest.clone()));
			// change for query_id2
			assert_eq!(Queries::<Test>::get(query_id2), Some(query_as_latest));
			assert!(Pallet::<Test>::do_try_state().is_ok());
		}

		// `LockedFungibles` migration
		{
			let account1 = AccountId::new([13u8; 32]);
			let account2 = AccountId::new([58u8; 32]);
			let unlocker = VersionedLocation::from(Location::parent());
			let lockeds_as_latest = BoundedVec::truncate_from(vec![(0, unlocker.clone())]);
			let lockeds_as_previous = BoundedVec::truncate_from(vec![(
				0,
				unlocker.into_version(previous_version).unwrap(),
			)]);
			assert_ne!(lockeds_as_latest, lockeds_as_previous);
			assert!(!lockeds_as_latest.needs_migration(latest_version));
			assert!(!lockeds_as_latest.needs_migration(previous_version));
			assert!(lockeds_as_previous.needs_migration(latest_version));
			assert!(!lockeds_as_previous.needs_migration(previous_version));

			// store two lockeds: migrated and not migrated
			LockedFungibles::<Test>::insert(&account1, lockeds_as_latest.clone());
			LockedFungibles::<Test>::insert(&account2, lockeds_as_previous);
			assert!(Pallet::<Test>::do_try_state().is_ok());

			// trigger migration
			Pallet::<Test>::migrate_data_to_xcm_version(&mut Weight::zero(), latest_version);

			// no change for account1
			assert_eq!(LockedFungibles::<Test>::get(&account1), Some(lockeds_as_latest.clone()));
			// change for account2
			assert_eq!(LockedFungibles::<Test>::get(&account2), Some(lockeds_as_latest));
			assert!(Pallet::<Test>::do_try_state().is_ok());
		}

		// `RemoteLockedFungibles` migration
		{
			let account1 = AccountId::new([13u8; 32]);
			let account2 = AccountId::new([58u8; 32]);
			let account3 = AccountId::new([97u8; 32]);
			let asset_id = VersionedAssetId::from(AssetId(Location::parent()));
			let owner = VersionedLocation::from(Location::parent());
			let locker = VersionedLocation::from(Location::parent());
			let key1_as_latest = (latest_version, account1, asset_id.clone());
			let key2_as_latest = (latest_version, account2, asset_id.clone());
			let key3_as_previous = (
				previous_version,
				account3.clone(),
				asset_id.clone().into_version(previous_version).unwrap(),
			);
			let expected_key3_as_latest = (latest_version, account3, asset_id);
			let data_as_latest = RemoteLockedFungibleRecord {
				amount: Default::default(),
				owner: owner.clone(),
				locker: locker.clone(),
				consumers: Default::default(),
			};
			let data_as_previous = RemoteLockedFungibleRecord {
				amount: Default::default(),
				owner: owner.into_version(previous_version).unwrap(),
				locker: locker.into_version(previous_version).unwrap(),
				consumers: Default::default(),
			};
			assert_ne!(data_as_latest.owner, data_as_previous.owner);
			assert_ne!(data_as_latest.locker, data_as_previous.locker);
			assert!(!key1_as_latest.needs_migration(latest_version));
			assert!(!key1_as_latest.needs_migration(previous_version));
			assert!(!key2_as_latest.needs_migration(latest_version));
			assert!(!key2_as_latest.needs_migration(previous_version));
			assert!(key3_as_previous.needs_migration(latest_version));
			assert!(!key3_as_previous.needs_migration(previous_version));
			assert!(!expected_key3_as_latest.needs_migration(latest_version));
			assert!(!expected_key3_as_latest.needs_migration(previous_version));
			assert!(!data_as_latest.needs_migration(latest_version));
			assert!(!data_as_latest.needs_migration(previous_version));
			assert!(data_as_previous.needs_migration(latest_version));
			assert!(!data_as_previous.needs_migration(previous_version));

			// store three lockeds:
			// fully migrated
			RemoteLockedFungibles::<Test>::insert(&key1_as_latest, data_as_latest.clone());
			// only key migrated
			RemoteLockedFungibles::<Test>::insert(&key2_as_latest, data_as_previous.clone());
			// neither key nor data migrated
			RemoteLockedFungibles::<Test>::insert(&key3_as_previous, data_as_previous);
			assert!(Pallet::<Test>::do_try_state().is_ok());

			// trigger migration
			Pallet::<Test>::migrate_data_to_xcm_version(&mut Weight::zero(), latest_version);

			let assert_locked_eq =
				|left: Option<RemoteLockedFungibleRecord<_, _>>,
				 right: Option<RemoteLockedFungibleRecord<_, _>>| {
					match (left, right) {
						(None, Some(_)) | (Some(_), None) => {
							assert!(false, "Received unexpected message")
						},
						(None, None) => (),
						(Some(l), Some(r)) => {
							assert_eq!(l.owner, r.owner);
							assert_eq!(l.locker, r.locker);
						},
					}
				};

			// no change
			assert_locked_eq(
				RemoteLockedFungibles::<Test>::get(&key1_as_latest),
				Some(data_as_latest.clone()),
			);
			// change - data migrated
			assert_locked_eq(
				RemoteLockedFungibles::<Test>::get(&key2_as_latest),
				Some(data_as_latest.clone()),
			);
			// fully migrated
			assert_locked_eq(RemoteLockedFungibles::<Test>::get(&key3_as_previous), None);
			assert_locked_eq(
				RemoteLockedFungibles::<Test>::get(&expected_key3_as_latest),
				Some(data_as_latest.clone()),
			);
			assert!(Pallet::<Test>::do_try_state().is_ok());
		}
	})
}

#[test]
fn record_xcm_works() {
	let balances = vec![(ALICE, INITIAL_BALANCE)];
	new_test_ext_with_balances(balances).execute_with(|| {
		let message = Xcm::<RuntimeCall>::builder()
			.withdraw_asset((Here, SEND_AMOUNT))
			.buy_execution((Here, SEND_AMOUNT), Unlimited)
			.deposit_asset(AllCounted(1), Junction::AccountId32 { network: None, id: BOB.into() })
			.build();
		// Test default values.
		assert_eq!(ShouldRecordXcm::<Test>::get(), false);
		assert_eq!(RecordedXcm::<Test>::get(), None);

		// By default the message won't be recorded.
		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(message.clone())),
			BaseXcmWeight::get() * 3,
		));
		assert_eq!(RecordedXcm::<Test>::get(), None);

		// We explicitly set the record flag to true so we record the XCM.
		ShouldRecordXcm::<Test>::put(true);
		assert_ok!(XcmPallet::execute(
			RuntimeOrigin::signed(ALICE),
			Box::new(VersionedXcm::from(message.clone())),
			BaseXcmWeight::get() * 3,
		));
		assert_eq!(RecordedXcm::<Test>::get(), Some(message.into()));
	});
}

#[test]
fn execute_initiate_transfer_and_check_sent_event() {
	use crate::Event;
	use sp_tracing::{
		test_log_capture::init_log_capture,
		tracing::{subscriber, Level},
	};

	let (log_capture, subscriber) = init_log_capture(Level::TRACE, true);
	subscriber::with_default(subscriber, || {
		let balances = vec![(ALICE, INITIAL_BALANCE)];
		new_test_ext_with_balances(balances).execute_with(|| {
			let beneficiary: Location =
				Location::new(1, [AccountId32 { network: None, id: BOB.into() }]);
			let fee_asset: Asset = (Parent, SEND_AMOUNT).into();

			let message = Xcm(vec![InitiateReserveWithdraw {
				assets: Wild(All),
				reserve: Parent.into(),
				xcm: Xcm(vec![
					BuyExecution { fees: fee_asset.clone(), weight_limit: Unlimited },
					DepositAsset { assets: All.into(), beneficiary: beneficiary.clone() },
				]),
			}]);

			let result = XcmPallet::execute(
				RuntimeOrigin::signed(ALICE),
				Box::new(VersionedXcm::from(message.clone())),
				BaseXcmWeight::get() * 3,
			);
			assert_ok!(result);

			let sent_msg_id = find_xcm_sent_message_id::<Test>(all_events()).unwrap();
			let sent_message: Xcm<()> = Xcm(vec![
				WithdrawAsset(Assets::new()),
				ClearOrigin,
				BuyExecution { fees: fee_asset.clone(), weight_limit: Unlimited },
				DepositAsset { assets: All.into(), beneficiary: beneficiary.clone() },
				SetTopic(sent_msg_id),
			]);
			assert!(log_capture
				.contains(format!("xcm::send: Sending msg msg={:?}", sent_message).as_str()));

			let origin: Location = AccountId32 { network: None, id: ALICE.into() }.into();
			assert_eq!(
				last_events(2),
				vec![
					RuntimeEvent::XcmPallet(Event::Sent {
						origin,
						destination: Parent.into(),
						message: Xcm::default(),
						message_id: sent_msg_id,
					}),
					RuntimeEvent::XcmPallet(Event::Attempted {
						outcome: Outcome::Complete { used: Weight::from_parts(1_000, 1_000) }
					}),
				]
			);
		})
	});
}

#[test]
fn deliver_failure_with_expect_error() {
	use sp_tracing::{
		test_log_capture::init_log_capture,
		tracing::{subscriber, Level},
	};

	let (log_capture, subscriber) = init_log_capture(Level::TRACE, true);
	subscriber::with_default(subscriber, || {
		let balances = vec![(ALICE, INITIAL_BALANCE)];

		new_test_ext_with_balances(balances).execute_with(|| {
			let message = Xcm(vec![InitiateReserveWithdraw {
				assets: Wild(All),
				reserve: Parent.into(),
				xcm: Xcm(vec![
					ExpectError(Some((1, xcm::latest::Error::Unimplemented)))
				]),
			}]);

			let result = XcmPallet::execute(
				RuntimeOrigin::signed(ALICE),
				Box::new(VersionedXcm::from(message.clone())),
				BaseXcmWeight::get() * 3,
			);

			// Expect an error from the send operation
			assert!(result.is_err());

			// Check logs for send attempt and failure
			assert!(log_capture.contains("xcm::send: Sending msg msg=Xcm([WithdrawAsset(Assets([])), ClearOrigin, ExpectError(Some((1, Unimplemented))), SetTopic("));
			assert!(log_capture.contains("xcm::send: XCM failed to deliver with error error=Transport(\"Intentional deliver failure used in tests\")"));
		})
	});
}

#[test]
fn query_weight_to_asset_fee_noop() {
	new_test_ext_with_balances(vec![]).execute_with(|| {
		let weight = Weight::from_parts(4_000_000_000, 3800);
		let asset_id = AssetId(Location::here());

		assert_storage_noop!(XcmPallet::query_weight_to_asset_fee::<Trader>(
			weight,
			asset_id.into()
		)
		.unwrap());
	})
}
