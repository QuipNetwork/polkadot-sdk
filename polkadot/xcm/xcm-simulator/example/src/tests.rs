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

use crate::*;

use codec::Encode;
use frame_support::{assert_ok, weights::Weight};
use xcm::latest::QueryResponseInfo;
use xcm_simulator::{mock_message_queue::ReceivedDmp, TestExt};

// Helper function for forming buy execution message
fn buy_execution<C>(fees: impl Into<Asset>) -> Instruction<C> {
	BuyExecution { fees: fees.into(), weight_limit: Unlimited }
}

/// Helper macro to check if a system event exists in the event list.
///
/// Example usage:
/// ```ignore
/// assert!(system_contains_event!(parachain, System(frame_system::Event::Remarked { .. })));
/// assert!(system_contains_event!(relay_chain, XcmPallet(pallet_xcm::Event::Attempted { .. })));
/// ```
macro_rules! system_contains_event {
    ($runtime:ident, $variant:ident($($pattern:tt)*)) => {
        $runtime::System::events().iter().any(|e| {
            matches!(e.event, $runtime::RuntimeEvent::$variant($($pattern)*))
        })
    };
}

#[test]
fn remote_account_ids_work() {
	child_account_account_id(1, ALICE);
	sibling_account_account_id(1, ALICE);
	parent_account_account_id(ALICE);
}

#[test]
fn dmp() {
	MockNet::reset();

	let remark = parachain::RuntimeCall::System(
		frame_system::Call::<parachain::Runtime>::remark_with_event { remark: vec![1, 2, 3] },
	);
	Relay::execute_with(|| {
		assert_ok!(RelayChainPalletXcm::send_xcm(
			Here,
			Parachain(1),
			Xcm(vec![Transact {
				origin_kind: OriginKind::SovereignAccount,
				call: remark.encode().into(),
				fallback_max_weight: None,
			}]),
		));
	});

	ParaA::execute_with(|| {
		assert!(system_contains_event!(parachain, System(frame_system::Event::Remarked { .. })));
	});
}

#[test]
fn ump() {
	MockNet::reset();

	let remark = relay_chain::RuntimeCall::System(
		frame_system::Call::<relay_chain::Runtime>::remark_with_event { remark: vec![1, 2, 3] },
	);
	ParaA::execute_with(|| {
		assert_ok!(ParachainPalletXcm::send_xcm(
			Here,
			Parent,
			Xcm(vec![Transact {
				origin_kind: OriginKind::SovereignAccount,
				call: remark.encode().into(),
				fallback_max_weight: None,
			}]),
		));
	});

	Relay::execute_with(|| {
		assert!(system_contains_event!(relay_chain, System(frame_system::Event::Remarked { .. })));
	});
}

#[test]
fn xcmp() {
	MockNet::reset();

	let remark = parachain::RuntimeCall::System(
		frame_system::Call::<parachain::Runtime>::remark_with_event { remark: vec![1, 2, 3] },
	);
	ParaA::execute_with(|| {
		assert_ok!(ParachainPalletXcm::send_xcm(
			Here,
			(Parent, Parachain(2)),
			Xcm(vec![Transact {
				origin_kind: OriginKind::SovereignAccount,
				call: remark.encode().into(),
				fallback_max_weight: None,
			}]),
		));
	});

	ParaB::execute_with(|| {
		assert!(system_contains_event!(parachain, System(frame_system::Event::Remarked { .. })));
	});
}

#[test]
fn reserve_transfer() {
	MockNet::reset();

	let withdraw_amount = 123;

	Relay::execute_with(|| {
		assert_ok!(RelayChainPalletXcm::limited_reserve_transfer_assets(
			relay_chain::RuntimeOrigin::signed(ALICE),
			Box::new(Parachain(1).into()),
			Box::new(AccountId32 { network: None, id: ALICE.into() }.into()),
			Box::new((Here, withdraw_amount).into()),
			0,
			Unlimited,
		));
		assert_eq!(
			relay_chain::Balances::free_balance(&child_account_id(1)),
			INITIAL_BALANCE + withdraw_amount
		);
		// Ensure expected events were emitted
		let attempted_emitted =
			system_contains_event!(relay_chain, XcmPallet(pallet_xcm::Event::Attempted { .. }));
		let sent_emitted =
			system_contains_event!(relay_chain, XcmPallet(pallet_xcm::Event::Sent { .. }));
		assert!(attempted_emitted, "Expected XcmPallet::Attempted event emitted");
		assert!(sent_emitted, "Expected XcmPallet::Sent event emitted");
	});

	ParaA::execute_with(|| {
		// free execution, full amount received
		assert_eq!(
			pallet_balances::Pallet::<parachain::Runtime>::free_balance(&ALICE),
			INITIAL_BALANCE + withdraw_amount
		);
	});
}

#[test]
fn reserve_transfer_with_error() {
	use sp_tracing::{
		test_log_capture::init_log_capture,
		tracing::{subscriber, Level},
	};

	// Reset the test network
	MockNet::reset();

	// Execute XCM Transfer and Capture Logs
	let (log_capture, subscriber) = init_log_capture(Level::ERROR, false);
	subscriber::with_default(subscriber, || {
		let invalid_dest = Box::new(Parachain(9999).into());
		let withdraw_amount = 123;

		Relay::execute_with(|| {
			let result = RelayChainPalletXcm::limited_reserve_transfer_assets(
				relay_chain::RuntimeOrigin::signed(ALICE),
				invalid_dest,
				Box::new(AccountId32 { network: None, id: ALICE.into() }.into()),
				Box::new((Here, withdraw_amount).into()),
				0,
				Unlimited,
			);

			// Ensure an error occurred
			assert!(result.is_err(), "Expected an error due to invalid destination");

			// Assert captured logs
			assert!(log_capture.contains("XCM validate_send failed"));

			// Verify that XcmPallet::Attempted was NOT emitted (rollback happened)
			let xcm_attempted_emitted =
				system_contains_event!(relay_chain, XcmPallet(pallet_xcm::Event::Attempted { .. }));
			assert!(
				!xcm_attempted_emitted,
				"Expected no XcmPallet::Attempted event due to rollback, but it was emitted"
			);
		});

		// Ensure no balance change due to the error
		ParaA::execute_with(|| {
			assert_eq!(
				pallet_balances::Pallet::<parachain::Runtime>::free_balance(&ALICE),
				INITIAL_BALANCE
			);
		});
	});
}

#[test]
fn remote_locking_and_unlocking() {
	MockNet::reset();

	let locked_amount = 100;

	ParaB::execute_with(|| {
		let message = Xcm(vec![LockAsset {
			asset: (Here, locked_amount).into(),
			unlocker: Parachain(1).into(),
		}]);
		assert_ok!(ParachainPalletXcm::send_xcm(Here, Parent, message.clone()));
	});

	Relay::execute_with(|| {
		use pallet_balances::{BalanceLock, Reasons};
		assert_eq!(
			relay_chain::Balances::locks(&child_account_id(2)),
			vec![BalanceLock { id: *b"py/xcmlk", amount: locked_amount, reasons: Reasons::All }]
		);
	});

	ParaA::execute_with(|| {
		assert_eq!(
			ReceivedDmp::<parachain::Runtime>::get(),
			vec![Xcm(vec![NoteUnlockable {
				owner: (Parent, Parachain(2)).into(),
				asset: (Parent, locked_amount).into()
			}])]
		);
	});

	ParaB::execute_with(|| {
		// Request unlocking part of the funds on the relay chain
		let message = Xcm(vec![RequestUnlock {
			asset: (Parent, locked_amount - 50).into(),
			locker: Parent.into(),
		}]);
		assert_ok!(ParachainPalletXcm::send_xcm(Here, (Parent, Parachain(1)), message));
	});

	Relay::execute_with(|| {
		use pallet_balances::{BalanceLock, Reasons};
		// Lock is reduced
		assert_eq!(
			relay_chain::Balances::locks(&child_account_id(2)),
			vec![BalanceLock {
				id: *b"py/xcmlk",
				amount: locked_amount - 50,
				reasons: Reasons::All
			}]
		);
	});
}

/// Scenario:
/// A parachain transfers an NFT resident on the relay chain to another parachain account.
///
/// Asserts that the parachain accounts are updated as expected.
#[test]
fn withdraw_and_deposit_nft() {
	MockNet::reset();

	Relay::execute_with(|| {
		assert_eq!(relay_chain::Uniques::owner(1, 42), Some(child_account_id(1)));
	});

	ParaA::execute_with(|| {
		let message = Xcm(vec![TransferAsset {
			assets: (GeneralIndex(1), 42u32).into(),
			beneficiary: Parachain(2).into(),
		}]);
		// Send withdraw and deposit
		assert_ok!(ParachainPalletXcm::send_xcm(Here, Parent, message));
	});

	Relay::execute_with(|| {
		assert_eq!(relay_chain::Uniques::owner(1, 42), Some(child_account_id(2)));
	});
}

/// Scenario:
/// The relay-chain teleports an NFT to a parachain.
///
/// Asserts that the parachain accounts are updated as expected.
#[test]
fn teleport_nft() {
	MockNet::reset();

	Relay::execute_with(|| {
		// Mint the NFT (1, 69) and give it to our "parachain#1 alias".
		assert_ok!(relay_chain::Uniques::mint(
			relay_chain::RuntimeOrigin::signed(ALICE),
			1,
			69,
			child_account_account_id(1, ALICE),
		));
		// The parachain#1 alias of Alice is what must hold it on the Relay-chain for it to be
		// withdrawable by Alice on the parachain.
		assert_eq!(relay_chain::Uniques::owner(1, 69), Some(child_account_account_id(1, ALICE)));
	});
	ParaA::execute_with(|| {
		assert_ok!(parachain::ForeignUniques::force_create(
			parachain::RuntimeOrigin::root(),
			(Parent, GeneralIndex(1)).into(),
			ALICE,
			false,
		));
		assert_eq!(
			parachain::ForeignUniques::owner((Parent, GeneralIndex(1)).into(), 69u32.into()),
			None,
		);
		assert_eq!(parachain::Balances::reserved_balance(&ALICE), 0);

		// IRL Alice would probably just execute this locally on the Relay-chain, but we can't
		// easily do that here since we only send between chains.
		let message = Xcm(vec![
			WithdrawAsset((GeneralIndex(1), 69u32).into()),
			InitiateTeleport {
				assets: AllCounted(1).into(),
				dest: Parachain(1).into(),
				xcm: Xcm(vec![DepositAsset {
					assets: AllCounted(1).into(),
					beneficiary: (AccountId32 { id: ALICE.into(), network: None },).into(),
				}]),
			},
		]);
		// Send teleport
		let alice = AccountId32 { id: ALICE.into(), network: None };
		assert_ok!(ParachainPalletXcm::send_xcm(alice, Parent, message));
	});
	ParaA::execute_with(|| {
		assert_eq!(
			parachain::ForeignUniques::owner((Parent, GeneralIndex(1)).into(), 69u32.into()),
			Some(ALICE),
		);
		assert_eq!(parachain::Balances::reserved_balance(&ALICE), 1000);
	});
	Relay::execute_with(|| {
		assert_eq!(relay_chain::Uniques::owner(1, 69), None);
	});
}

/// Scenario:
/// The relay-chain transfers an NFT into a parachain's sovereign account, who then mints a
/// trustless-backed-derived locally.
///
/// Asserts that the parachain accounts are updated as expected.
#[test]
fn reserve_asset_transfer_nft() {
	sp_tracing::init_for_tests();
	MockNet::reset();

	Relay::execute_with(|| {
		assert_ok!(relay_chain::Uniques::force_create(
			relay_chain::RuntimeOrigin::root(),
			2,
			ALICE,
			false
		));
		assert_ok!(relay_chain::Uniques::mint(
			relay_chain::RuntimeOrigin::signed(ALICE),
			2,
			69,
			child_account_account_id(1, ALICE)
		));
		assert_eq!(relay_chain::Uniques::owner(2, 69), Some(child_account_account_id(1, ALICE)));
	});
	ParaA::execute_with(|| {
		assert_ok!(parachain::ForeignUniques::force_create(
			parachain::RuntimeOrigin::root(),
			(Parent, GeneralIndex(2)).into(),
			ALICE,
			false,
		));
		assert_eq!(
			parachain::ForeignUniques::owner((Parent, GeneralIndex(2)).into(), 69u32.into()),
			None,
		);
		assert_eq!(parachain::Balances::reserved_balance(&ALICE), 0);

		let message = Xcm(vec![
			WithdrawAsset((GeneralIndex(2), 69u32).into()),
			DepositReserveAsset {
				assets: AllCounted(1).into(),
				dest: Parachain(1).into(),
				xcm: Xcm(vec![DepositAsset {
					assets: AllCounted(1).into(),
					beneficiary: (AccountId32 { id: ALICE.into(), network: None },).into(),
				}]),
			},
		]);
		// Send transfer
		let alice = AccountId32 { id: ALICE.into(), network: None };
		assert_ok!(ParachainPalletXcm::send_xcm(alice, Parent, message));
	});
	ParaA::execute_with(|| {
		tracing::debug!(target: "xcm-executor", "Hello");
		assert_eq!(
			parachain::ForeignUniques::owner((Parent, GeneralIndex(2)).into(), 69u32.into()),
			Some(ALICE),
		);
		assert_eq!(parachain::Balances::reserved_balance(&ALICE), 1000);
	});

	Relay::execute_with(|| {
		assert_eq!(relay_chain::Uniques::owner(2, 69), Some(child_account_id(1)));
	});
}

/// Scenario:
/// The relay-chain creates an asset class on a parachain and then Alice transfers her NFT into
/// that parachain's sovereign account, who then mints a trustless-backed-derivative locally.
///
/// Asserts that the parachain accounts are updated as expected.
#[test]
fn reserve_asset_class_create_and_reserve_transfer() {
	MockNet::reset();

	Relay::execute_with(|| {
		assert_ok!(relay_chain::Uniques::force_create(
			relay_chain::RuntimeOrigin::root(),
			2,
			ALICE,
			false
		));
		assert_ok!(relay_chain::Uniques::mint(
			relay_chain::RuntimeOrigin::signed(ALICE),
			2,
			69,
			child_account_account_id(1, ALICE)
		));
		assert_eq!(relay_chain::Uniques::owner(2, 69), Some(child_account_account_id(1, ALICE)));

		let message = Xcm(vec![Transact {
			origin_kind: OriginKind::Xcm,
			call: parachain::RuntimeCall::from(
				pallet_uniques::Call::<parachain::Runtime>::create {
					collection: (Parent, 2u64).into(),
					admin: parent_account_id(),
				},
			)
			.encode()
			.into(),
			fallback_max_weight: None,
		}]);
		// Send creation.
		assert_ok!(RelayChainPalletXcm::send_xcm(Here, Parachain(1), message));
	});
	ParaA::execute_with(|| {
		// Then transfer
		let message = Xcm(vec![
			WithdrawAsset((GeneralIndex(2), 69u32).into()),
			DepositReserveAsset {
				assets: AllCounted(1).into(),
				dest: Parachain(1).into(),
				xcm: Xcm(vec![DepositAsset {
					assets: AllCounted(1).into(),
					beneficiary: (AccountId32 { id: ALICE.into(), network: None },).into(),
				}]),
			},
		]);
		let alice = AccountId32 { id: ALICE.into(), network: None };
		assert_ok!(ParachainPalletXcm::send_xcm(alice, Parent, message));
	});
	ParaA::execute_with(|| {
		assert_eq!(parachain::Balances::reserved_balance(&parent_account_id()), 1000);
		assert_eq!(
			parachain::ForeignUniques::collection_owner((Parent, 2u64).into()),
			Some(parent_account_id())
		);
	});
}

/// Scenario:
/// A parachain transfers funds on the relay chain to another parachain account.
///
/// Asserts that the parachain accounts are updated as expected.
#[test]
fn withdraw_and_deposit() {
	MockNet::reset();

	let send_amount = 10;

	ParaA::execute_with(|| {
		let message = Xcm(vec![
			WithdrawAsset((Here, send_amount).into()),
			buy_execution((Here, send_amount)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: Parachain(2).into() },
		]);
		// Send withdraw and deposit
		assert_ok!(ParachainPalletXcm::send_xcm(Here, Parent, message.clone()));
	});

	Relay::execute_with(|| {
		assert_eq!(
			relay_chain::Balances::free_balance(child_account_id(1)),
			INITIAL_BALANCE - send_amount
		);
		assert_eq!(
			relay_chain::Balances::free_balance(child_account_id(2)),
			INITIAL_BALANCE + send_amount
		);
	});
}

/// Scenario:
/// A parachain wants to be notified that a transfer worked correctly.
/// It sends a `QueryHolding` after the deposit to get notified on success.
///
/// Asserts that the balances are updated correctly and the expected XCM is sent.
#[test]
fn query_holding() {
	MockNet::reset();

	let send_amount = 10;
	let query_id_set = 1234;

	// Send a message which fully succeeds on the relay chain
	let expected_hash = ParaA::execute_with(|| {
		let message = Xcm(vec![
			WithdrawAsset((Here, send_amount).into()),
			buy_execution((Here, send_amount)),
			DepositAsset { assets: AllCounted(1).into(), beneficiary: Parachain(2).into() },
			ReportHolding {
				response_info: QueryResponseInfo {
					destination: Parachain(1).into(),
					query_id: query_id_set,
					max_weight: Weight::from_parts(1_000_000_000, 1024 * 1024),
				},
				assets: All.into(),
			},
		]);
		// Send withdraw and deposit with query holding
		assert_ok!(ParachainPalletXcm::send_xcm(Here, Parent, message.clone(),));

		VersionedXcm::from(message).using_encoded(sp_crypto_hashing::blake2_256)
	});

	// Check that transfer was executed
	Relay::execute_with(|| {
		// Withdraw executed
		assert_eq!(
			relay_chain::Balances::free_balance(child_account_id(1)),
			INITIAL_BALANCE - send_amount
		);
		// Deposit executed
		assert_eq!(
			relay_chain::Balances::free_balance(child_account_id(2)),
			INITIAL_BALANCE + send_amount
		);
	});

	// Check that QueryResponse message was received
	ParaA::execute_with(|| {
		assert_eq!(
			ReceivedDmp::<parachain::Runtime>::get(),
			vec![Xcm(vec![
				QueryResponse {
					query_id: query_id_set,
					response: Response::Assets(Assets::new()),
					max_weight: Weight::from_parts(1_000_000_000, 1024 * 1024),
					querier: Some(Here.into()),
				},
				SetTopic(expected_hash),
			])],
		);
	});
}
