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

use codec::Encode;
use frame_support::sp_runtime::traits::Dispatchable;
use parachains_common::AccountId;
use people_westend_runtime::people::IdentityInfo;
use westend_runtime::Dmp;
use westend_system_emulated_network::people_westend_emulated_chain::people_westend_runtime;

use pallet_identity::Data;

use emulated_integration_tests_common::accounts::{ALICE, BOB};

#[test]
fn relay_commands_add_registrar() {
	let (origin_kind, origin) = (OriginKind::Superuser, <Westend as Chain>::RuntimeOrigin::root());

	let registrar: AccountId = [1; 32].into();
	Westend::execute_with(|| {
		type Runtime = <Westend as Chain>::Runtime;
		type RuntimeCall = <Westend as Chain>::RuntimeCall;
		type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
		type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
		type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

		Dmp::make_parachain_reachable(1004);

		let add_registrar_call =
			PeopleCall::Identity(pallet_identity::Call::<PeopleRuntime>::add_registrar {
				account: registrar.into(),
			});

		let xcm_message = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
			dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
			message: bx!(VersionedXcm::from(Xcm(vec![
				UnpaidExecution { weight_limit: Unlimited, check_origin: None },
				Transact {
					origin_kind,
					call: add_registrar_call.encode().into(),
					fallback_max_weight: None
				}
			]))),
		});

		assert_ok!(xcm_message.dispatch(origin));

		assert_expected_events!(
			Westend,
			vec![
				RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
			]
		);
	});

	PeopleWestend::execute_with(|| {
		type RuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			PeopleWestend,
			vec![
				RuntimeEvent::Identity(pallet_identity::Event::RegistrarAdded { .. }) => {},
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: true, .. }) => {},
			]
		);
	});
}

#[test]
fn relay_commands_add_registrar_wrong_origin() {
	let people_westend_alice = PeopleWestend::account_id_of(ALICE);

	let origins = vec![(
		OriginKind::SovereignAccount,
		<Westend as Chain>::RuntimeOrigin::signed(people_westend_alice),
	)];

	for (origin_kind, origin) in origins {
		let registrar: AccountId = [1; 32].into();
		Westend::execute_with(|| {
			type Runtime = <Westend as Chain>::Runtime;
			type RuntimeCall = <Westend as Chain>::RuntimeCall;
			type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
			type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
			type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

			Dmp::make_parachain_reachable(1004);

			let add_registrar_call =
				PeopleCall::Identity(pallet_identity::Call::<PeopleRuntime>::add_registrar {
					account: registrar.into(),
				});

			let xcm_message = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
				dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
				message: bx!(VersionedXcm::from(Xcm(vec![
					UnpaidExecution { weight_limit: Unlimited, check_origin: None },
					Transact {
						origin_kind,
						call: add_registrar_call.encode().into(),
						fallback_max_weight: None
					}
				]))),
			});

			assert_ok!(xcm_message.dispatch(origin));
			assert_expected_events!(
				Westend,
				vec![
					RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
				]
			);
		});

		PeopleWestend::execute_with(|| {
			type RuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;

			assert_expected_events!(
				PeopleWestend,
				vec![
					RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: false, .. }) => {},
				]
			);
		});
	}
}

#[test]
fn relay_commands_kill_identity() {
	// To kill an identity, first one must be set
	PeopleWestend::execute_with(|| {
		type PeopleRuntime = <PeopleWestend as Chain>::Runtime;
		type PeopleRuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;

		let people_westend_alice =
			<PeopleWestend as Chain>::RuntimeOrigin::signed(PeopleWestend::account_id_of(ALICE));

		let identity_info = IdentityInfo {
			email: Data::Raw(b"test@test.io".to_vec().try_into().unwrap()),
			..Default::default()
		};
		let identity: Box<<PeopleRuntime as pallet_identity::Config>::IdentityInformation> =
			Box::new(identity_info);

		assert_ok!(<PeopleWestend as PeopleWestendPallet>::Identity::set_identity(
			people_westend_alice,
			identity
		));

		assert_expected_events!(
			PeopleWestend,
			vec![
				PeopleRuntimeEvent::Identity(pallet_identity::Event::IdentitySet { .. }) => {},
			]
		);
	});

	let (origin_kind, origin) = (OriginKind::Superuser, <Westend as Chain>::RuntimeOrigin::root());

	Westend::execute_with(|| {
		type Runtime = <Westend as Chain>::Runtime;
		type RuntimeCall = <Westend as Chain>::RuntimeCall;
		type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
		type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
		type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

		Dmp::make_parachain_reachable(1004);

		let kill_identity_call =
			PeopleCall::Identity(pallet_identity::Call::<PeopleRuntime>::kill_identity {
				target: people_westend_runtime::MultiAddress::Id(PeopleWestend::account_id_of(
					ALICE,
				)),
			});

		let xcm_message = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
			dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
			message: bx!(VersionedXcm::from(Xcm(vec![
				UnpaidExecution { weight_limit: Unlimited, check_origin: None },
				Transact {
					origin_kind,
					call: kill_identity_call.encode().into(),
					fallback_max_weight: None
				}
			]))),
		});

		assert_ok!(xcm_message.dispatch(origin));

		assert_expected_events!(
			Westend,
			vec![
				RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
			]
		);
	});

	PeopleWestend::execute_with(|| {
		type RuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			PeopleWestend,
			vec![
				RuntimeEvent::Identity(pallet_identity::Event::IdentityKilled { .. }) => {},
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: true, .. }) => {},
			]
		);
	});
}

#[test]
fn relay_commands_kill_identity_wrong_origin() {
	let people_westend_alice = PeopleWestend::account_id_of(BOB);

	let origins = vec![(
		OriginKind::SovereignAccount,
		<Westend as Chain>::RuntimeOrigin::signed(people_westend_alice),
	)];

	for (origin_kind, origin) in origins {
		Westend::execute_with(|| {
			type Runtime = <Westend as Chain>::Runtime;
			type RuntimeCall = <Westend as Chain>::RuntimeCall;
			type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
			type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
			type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

			Dmp::make_parachain_reachable(1004);

			let kill_identity_call =
				PeopleCall::Identity(pallet_identity::Call::<PeopleRuntime>::kill_identity {
					target: people_westend_runtime::MultiAddress::Id(PeopleWestend::account_id_of(
						ALICE,
					)),
				});

			let xcm_message = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
				dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
				message: bx!(VersionedXcm::from(Xcm(vec![
					UnpaidExecution { weight_limit: Unlimited, check_origin: None },
					Transact {
						origin_kind,
						call: kill_identity_call.encode().into(),
						fallback_max_weight: None
					}
				]))),
			});

			assert_ok!(xcm_message.dispatch(origin));
			assert_expected_events!(
				Westend,
				vec![
					RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
				]
			);
		});

		PeopleWestend::execute_with(|| {
			assert_expected_events!(PeopleWestend, vec![]);
		});
	}
}

#[test]
fn relay_commands_add_remove_username_authority() {
	let people_westend_alice = PeopleWestend::account_id_of(ALICE);
	let people_westend_bob = PeopleWestend::account_id_of(BOB);

	let (origin_kind, origin, usr) =
		(OriginKind::Superuser, <Westend as Chain>::RuntimeOrigin::root(), "rootusername");

	// First, add a username authority.
	Westend::execute_with(|| {
		type Runtime = <Westend as Chain>::Runtime;
		type RuntimeCall = <Westend as Chain>::RuntimeCall;
		type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
		type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
		type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

		Dmp::make_parachain_reachable(1004);

		let add_username_authority =
			PeopleCall::Identity(pallet_identity::Call::<PeopleRuntime>::add_username_authority {
				authority: people_westend_runtime::MultiAddress::Id(people_westend_alice.clone()),
				suffix: b"suffix1".into(),
				allocation: 10,
			});

		let add_authority_xcm_msg = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
			dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
			message: bx!(VersionedXcm::from(Xcm(vec![
				UnpaidExecution { weight_limit: Unlimited, check_origin: None },
				Transact {
					origin_kind,
					call: add_username_authority.encode().into(),
					fallback_max_weight: None
				}
			]))),
		});

		assert_ok!(add_authority_xcm_msg.dispatch(origin.clone()));

		assert_expected_events!(
			Westend,
			vec![
				RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
			]
		);
	});

	// Check events system-parachain-side
	PeopleWestend::execute_with(|| {
		type RuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			PeopleWestend,
			vec![
				RuntimeEvent::Identity(pallet_identity::Event::AuthorityAdded { .. }) => {},
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: true, .. }) => {},
			]
		);
	});

	// Now, use the previously added username authority to concede a username to an account.
	PeopleWestend::execute_with(|| {
		type PeopleRuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;
		let full_username = [usr.to_owned(), ".suffix1".to_owned()].concat().into_bytes();

		assert_ok!(<PeopleWestend as PeopleWestendPallet>::Identity::set_username_for(
			<PeopleWestend as Chain>::RuntimeOrigin::signed(people_westend_alice.clone()),
			people_westend_runtime::MultiAddress::Id(people_westend_bob.clone()),
			full_username,
			None,
			true
		));

		assert_expected_events!(
			PeopleWestend,
			vec![
				PeopleRuntimeEvent::Identity(pallet_identity::Event::UsernameQueued { .. }) => {},
			]
		);
	});

	// Accept the given username
	PeopleWestend::execute_with(|| {
		type PeopleRuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;
		let full_username = [usr.to_owned(), ".suffix1".to_owned()].concat().into_bytes();

		assert_ok!(<PeopleWestend as PeopleWestendPallet>::Identity::accept_username(
			<PeopleWestend as Chain>::RuntimeOrigin::signed(people_westend_bob.clone()),
			full_username.try_into().unwrap(),
		));

		assert_expected_events!(
			PeopleWestend,
			vec![
				PeopleRuntimeEvent::Identity(pallet_identity::Event::UsernameSet { .. }) => {},
			]
		);
	});

	// Now, remove the username authority with another privileged XCM call.
	Westend::execute_with(|| {
		type Runtime = <Westend as Chain>::Runtime;
		type RuntimeCall = <Westend as Chain>::RuntimeCall;
		type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
		type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
		type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

		Dmp::make_parachain_reachable(1004);

		let remove_username_authority = PeopleCall::Identity(pallet_identity::Call::<
			PeopleRuntime,
		>::remove_username_authority {
			authority: people_westend_runtime::MultiAddress::Id(people_westend_alice.clone()),
			suffix: b"suffix1".into(),
		});

		let remove_authority_xcm_msg = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
			dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
			message: bx!(VersionedXcm::from(Xcm(vec![
				UnpaidExecution { weight_limit: Unlimited, check_origin: None },
				Transact {
					origin_kind,
					call: remove_username_authority.encode().into(),
					fallback_max_weight: None
				}
			]))),
		});

		assert_ok!(remove_authority_xcm_msg.dispatch(origin));

		assert_expected_events!(
			Westend,
			vec![
				RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
			]
		);
	});

	// Final event check.
	PeopleWestend::execute_with(|| {
		type RuntimeEvent = <PeopleWestend as Chain>::RuntimeEvent;

		assert_expected_events!(
			PeopleWestend,
			vec![
				RuntimeEvent::Identity(pallet_identity::Event::AuthorityRemoved { .. }) => {},
				RuntimeEvent::MessageQueue(pallet_message_queue::Event::Processed { success: true, .. }) => {},
			]
		);
	});
}

#[test]
fn relay_commands_add_remove_username_authority_wrong_origin() {
	let people_westend_alice = PeopleWestend::account_id_of(ALICE);

	let origins = vec![(
		OriginKind::SovereignAccount,
		<Westend as Chain>::RuntimeOrigin::signed(people_westend_alice.clone()),
	)];

	for (origin_kind, origin) in origins {
		Westend::execute_with(|| {
			type Runtime = <Westend as Chain>::Runtime;
			type RuntimeCall = <Westend as Chain>::RuntimeCall;
			type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
			type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
			type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

			Dmp::make_parachain_reachable(1004);

			let add_username_authority = PeopleCall::Identity(pallet_identity::Call::<
				PeopleRuntime,
			>::add_username_authority {
				authority: people_westend_runtime::MultiAddress::Id(people_westend_alice.clone()),
				suffix: b"suffix1".into(),
				allocation: 10,
			});

			let add_authority_xcm_msg = RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
				dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
				message: bx!(VersionedXcm::from(Xcm(vec![
					UnpaidExecution { weight_limit: Unlimited, check_origin: None },
					Transact {
						origin_kind,
						call: add_username_authority.encode().into(),
						fallback_max_weight: None
					}
				]))),
			});

			assert_ok!(add_authority_xcm_msg.dispatch(origin.clone()));
			assert_expected_events!(
				Westend,
				vec![
					RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
				]
			);
		});

		// Check events system-parachain-side
		PeopleWestend::execute_with(|| {
			assert_expected_events!(PeopleWestend, vec![]);
		});

		Westend::execute_with(|| {
			type Runtime = <Westend as Chain>::Runtime;
			type RuntimeCall = <Westend as Chain>::RuntimeCall;
			type RuntimeEvent = <Westend as Chain>::RuntimeEvent;
			type PeopleCall = <PeopleWestend as Chain>::RuntimeCall;
			type PeopleRuntime = <PeopleWestend as Chain>::Runtime;

			let remove_username_authority = PeopleCall::Identity(pallet_identity::Call::<
				PeopleRuntime,
			>::remove_username_authority {
				authority: people_westend_runtime::MultiAddress::Id(people_westend_alice.clone()),
				suffix: b"suffix1".into(),
			});

			Dmp::make_parachain_reachable(1004);

			let remove_authority_xcm_msg =
				RuntimeCall::XcmPallet(pallet_xcm::Call::<Runtime>::send {
					dest: bx!(VersionedLocation::from(Location::new(0, [Parachain(1004)]))),
					message: bx!(VersionedXcm::from(Xcm(vec![
						UnpaidExecution { weight_limit: Unlimited, check_origin: None },
						Transact {
							origin_kind: OriginKind::SovereignAccount,
							call: remove_username_authority.encode().into(),
							fallback_max_weight: None,
						}
					]))),
				});

			assert_ok!(remove_authority_xcm_msg.dispatch(origin));
			assert_expected_events!(
				Westend,
				vec![
					RuntimeEvent::XcmPallet(pallet_xcm::Event::Sent { .. }) => {},
				]
			);
		});

		PeopleWestend::execute_with(|| {
			assert_expected_events!(PeopleWestend, vec![]);
		});
	}
}
