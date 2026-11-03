// This file is part of Substrate.

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

use crate::*;
use frame_support::{
	assert_noop, assert_ok,
	dispatch::{Pays, PostDispatchInfo, WithPostDispatchInfo},
	storage,
	traits::{OnRuntimeUpgrade, WhitelistedStorageKeys},
};
use mock::{RuntimeOrigin, *};
use sp_core::{hexdisplay::HexDisplay, storage::well_known_keys, H256};
use sp_runtime::{
	generic::{Digest, DigestItem},
	traits::{BlakeTwo256, Header},
	DispatchError, DispatchErrorWithPostInfo,
};
use std::collections::BTreeSet;
use substrate_test_runtime_client::WasmExecutor;

#[test]
fn check_whitelist() {
	let whitelist: BTreeSet<String> = AllPalletsWithSystem::whitelisted_storage_keys()
		.iter()
		.map(|s| HexDisplay::from(&s.key).to_string())
		.collect();

	// Block Number
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef702a5c1b19ab7a04f536c519aca4983ac"));
	// Execution Phase
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef7ff553b5a9862a516939d82b3d3d8661a"));
	// Event Count
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef70a98fdbe9ce6c55837576c60c7af3850"));
	// System Events
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef780d41e5e16056765bc8461851072c9d7"));
	// System BlockWeight
	assert!(whitelist.contains("26aa394eea5630e07c48ae0c9558cef734abf5cb34d6244378cddbf18e849d96"));
}

#[test]
fn origin_works() {
	let o = RuntimeOrigin::from(RawOrigin::<u64>::Signed(1u64));
	let x: Result<RawOrigin<u64>, RuntimeOrigin> = o.into();
	assert_eq!(x.unwrap(), RawOrigin::<u64>::Signed(1u64));
}

#[test]
fn unique_datum_works() {
	new_test_ext().execute_with(|| {
		System::initialize(&1, &[0u8; 32].into(), &Default::default());
		assert!(sp_io::storage::exists(well_known_keys::INTRABLOCK_ENTROPY));

		let h1 = unique(b"");
		assert_eq!(
			32,
			sp_io::storage::read(well_known_keys::INTRABLOCK_ENTROPY, &mut [], 0).unwrap()
		);
		let h2 = unique(b"");
		assert_eq!(
			32,
			sp_io::storage::read(well_known_keys::INTRABLOCK_ENTROPY, &mut [], 0).unwrap()
		);
		assert_ne!(h1, h2);

		let h3 = unique(b"Hello");
		assert_eq!(
			32,
			sp_io::storage::read(well_known_keys::INTRABLOCK_ENTROPY, &mut [], 0).unwrap()
		);
		assert_ne!(h2, h3);

		let h4 = unique(b"Hello");
		assert_eq!(
			32,
			sp_io::storage::read(well_known_keys::INTRABLOCK_ENTROPY, &mut [], 0).unwrap()
		);
		assert_ne!(h3, h4);

		System::finalize();
		assert!(!sp_io::storage::exists(well_known_keys::INTRABLOCK_ENTROPY));
	});
}

#[test]
fn stored_map_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(System::inc_providers(&0), IncRefStatus::Created);
		assert_ok!(System::insert(&0, 42));
		assert!(!System::is_provider_required(&0));

		assert_eq!(
			Account::<Test>::get(0),
			AccountInfo {
				nonce: 0u64.into(),
				providers: 1,
				consumers: 0,
				sufficients: 0,
				data: 42
			}
		);

		assert_ok!(System::inc_consumers(&0));
		assert!(System::is_provider_required(&0));

		assert_ok!(System::insert(&0, 69));
		assert!(System::is_provider_required(&0));

		System::dec_consumers(&0);
		assert!(!System::is_provider_required(&0));

		assert!(Killed::get().is_empty());
		assert_ok!(System::remove(&0));
		assert_ok!(System::dec_providers(&0));
		assert_eq!(Killed::get(), vec![0u64]);
	});
}

#[test]
fn provider_ref_handover_to_self_sufficient_ref_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(System::inc_providers(&0), IncRefStatus::Created);
		System::inc_account_nonce(&0);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// a second reference coming and going doesn't change anything.
		assert_eq!(System::inc_sufficients(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_sufficients(&0), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// a provider reference coming and going doesn't change anything.
		assert_eq!(System::inc_providers(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_providers(&0).unwrap(), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// decreasing the providers with a self-sufficient present should not delete the account
		assert_eq!(System::inc_sufficients(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_providers(&0).unwrap(), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// decreasing the sufficients should delete the account
		assert_eq!(System::dec_sufficients(&0), DecRefStatus::Reaped);
		assert_eq!(System::account_nonce(&0), 0u64.into());
	});
}

#[test]
fn dec_sufficients_does_not_undeflow() {
	new_test_ext().execute_with(|| {
		assert_eq!(System::inc_providers(&0), IncRefStatus::Created);
		assert_eq!(System::dec_sufficients(&0), DecRefStatus::Exists);
	});
}

#[test]
fn self_sufficient_ref_handover_to_provider_ref_works() {
	new_test_ext().execute_with(|| {
		assert_eq!(System::inc_sufficients(&0), IncRefStatus::Created);
		System::inc_account_nonce(&0);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// a second reference coming and going doesn't change anything.
		assert_eq!(System::inc_providers(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_providers(&0).unwrap(), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// a sufficient reference coming and going doesn't change anything.
		assert_eq!(System::inc_sufficients(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_sufficients(&0), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// decreasing the sufficients with a provider present should not delete the account
		assert_eq!(System::inc_providers(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_sufficients(&0), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		// decreasing the providers should delete the account
		assert_eq!(System::dec_providers(&0).unwrap(), DecRefStatus::Reaped);
		assert_eq!(System::account_nonce(&0), 0u64.into());
	});
}

#[test]
fn sufficient_cannot_support_consumer() {
	new_test_ext().execute_with(|| {
		assert_eq!(System::inc_sufficients(&0), IncRefStatus::Created);
		System::inc_account_nonce(&0);
		assert_eq!(System::account_nonce(&0), 1u64.into());
		assert_noop!(System::inc_consumers(&0), DispatchError::NoProviders);

		assert_eq!(System::inc_providers(&0), IncRefStatus::Existed);
		assert_ok!(System::inc_consumers(&0));
		assert_noop!(System::dec_providers(&0), DispatchError::ConsumerRemaining);
	});
}

#[test]
fn provider_required_to_support_consumer() {
	new_test_ext().execute_with(|| {
		assert_noop!(System::inc_consumers(&0), DispatchError::NoProviders);

		assert_eq!(System::inc_providers(&0), IncRefStatus::Created);
		System::inc_account_nonce(&0);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		assert_eq!(System::inc_providers(&0), IncRefStatus::Existed);
		assert_eq!(System::dec_providers(&0).unwrap(), DecRefStatus::Exists);
		assert_eq!(System::account_nonce(&0), 1u64.into());

		assert_ok!(System::inc_consumers(&0));
		assert_noop!(System::dec_providers(&0), DispatchError::ConsumerRemaining);

		System::dec_consumers(&0);
		assert_eq!(System::dec_providers(&0).unwrap(), DecRefStatus::Reaped);
		assert_eq!(System::account_nonce(&0), 0u64.into());
	});
}

#[test]
fn deposit_event_should_work() {
	new_test_ext().execute_with(|| {
		System::reset_events();
		System::initialize(&1, &[0u8; 32].into(), &Default::default());
		System::note_finished_extrinsics();
		System::deposit_event(SysEvent::CodeUpdated { hash: Default::default() });
		System::finalize();
		assert_eq!(
			System::events(),
			vec![EventRecord {
				phase: Phase::Finalization,
				event: SysEvent::CodeUpdated { hash: Default::default() }.into(),
				topics: vec![],
			}]
		);

		let normal_base = <Test as crate::Config>::BlockWeights::get()
			.get(DispatchClass::Normal)
			.base_extrinsic;

		System::reset_events();
		System::initialize(&2, &[0u8; 32].into(), &Default::default());
		System::deposit_event(SysEvent::NewAccount { account: 32 });
		System::note_finished_initialize();
		System::deposit_event(SysEvent::KilledAccount { account: 42 });
		System::note_applied_extrinsic(&Ok(().into()), Default::default());
		System::note_applied_extrinsic(&Err(DispatchError::BadOrigin.into()), Default::default());
		System::note_finished_extrinsics();
		System::deposit_event(SysEvent::NewAccount { account: 3 });
		System::finalize();
		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Initialization,
					event: SysEvent::NewAccount { account: 32 }.into(),
					topics: vec![],
				},
				EventRecord {
					phase: Phase::ApplyExtrinsic(0),
					event: SysEvent::KilledAccount { account: 42 }.into(),
					topics: vec![]
				},
				EventRecord {
					phase: Phase::ApplyExtrinsic(0),
					event: SysEvent::ExtrinsicSuccess {
						dispatch_info: DispatchEventInfo {
							weight: normal_base,
							..Default::default()
						}
					}
					.into(),
					topics: vec![]
				},
				EventRecord {
					phase: Phase::ApplyExtrinsic(1),
					event: SysEvent::ExtrinsicFailed {
						dispatch_error: DispatchError::BadOrigin.into(),
						dispatch_info: DispatchEventInfo {
							weight: normal_base,
							..Default::default()
						}
					}
					.into(),
					topics: vec![]
				},
				EventRecord {
					phase: Phase::Finalization,
					event: SysEvent::NewAccount { account: 3 }.into(),
					topics: vec![]
				},
			]
		);
	});
}

#[test]
fn deposit_event_uses_actual_weight_and_pays_fee() {
	new_test_ext().execute_with(|| {
		System::reset_events();
		System::initialize(&1, &[0u8; 32].into(), &Default::default());
		System::note_finished_initialize();

		let normal_base = <Test as crate::Config>::BlockWeights::get()
			.get(DispatchClass::Normal)
			.base_extrinsic;
		let pre_info =
			DispatchInfo { call_weight: Weight::from_parts(1000, 0), ..Default::default() };
		System::note_applied_extrinsic(&Ok(from_actual_ref_time(Some(300))), pre_info);
		System::note_applied_extrinsic(&Ok(from_actual_ref_time(Some(1000))), pre_info);
		System::note_applied_extrinsic(
			// values over the pre info should be capped at pre dispatch value
			&Ok(from_actual_ref_time(Some(1200))),
			pre_info,
		);
		System::note_applied_extrinsic(
			&Ok(from_post_weight_info(Some(2_500_000), Pays::Yes)),
			pre_info,
		);
		System::note_applied_extrinsic(&Ok(Pays::No.into()), pre_info);
		System::note_applied_extrinsic(
			&Ok(from_post_weight_info(Some(2_500_000), Pays::No)),
			pre_info,
		);
		System::note_applied_extrinsic(&Ok(from_post_weight_info(Some(500), Pays::No)), pre_info);
		System::note_applied_extrinsic(
			&Err(DispatchError::BadOrigin.with_weight(Weight::from_parts(999, 0))),
			pre_info,
		);

		System::note_applied_extrinsic(
			&Err(DispatchErrorWithPostInfo {
				post_info: PostDispatchInfo { actual_weight: None, pays_fee: Pays::Yes },
				error: DispatchError::BadOrigin,
			}),
			pre_info,
		);
		System::note_applied_extrinsic(
			&Err(DispatchErrorWithPostInfo {
				post_info: PostDispatchInfo {
					actual_weight: Some(Weight::from_parts(800, 0)),
					pays_fee: Pays::Yes,
				},
				error: DispatchError::BadOrigin,
			}),
			pre_info,
		);
		System::note_applied_extrinsic(
			&Err(DispatchErrorWithPostInfo {
				post_info: PostDispatchInfo {
					actual_weight: Some(Weight::from_parts(800, 0)),
					pays_fee: Pays::No,
				},
				error: DispatchError::BadOrigin,
			}),
			pre_info,
		);
		// Also works for operational.
		let operational_base = <Test as crate::Config>::BlockWeights::get()
			.get(DispatchClass::Operational)
			.base_extrinsic;
		assert!(normal_base != operational_base, "Test pre-condition violated");
		let pre_info = DispatchInfo {
			call_weight: Weight::from_parts(1000, 0),
			class: DispatchClass::Operational,
			..Default::default()
		};
		System::note_applied_extrinsic(&Ok(from_actual_ref_time(Some(300))), pre_info);

		let got = System::events();
		let want = vec![
			EventRecord {
				phase: Phase::ApplyExtrinsic(0),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(300, 0).saturating_add(normal_base),
						..Default::default()
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(1),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(1000, 0).saturating_add(normal_base),
						..Default::default()
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(2),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(1000, 0).saturating_add(normal_base),
						..Default::default()
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(3),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(1000, 0).saturating_add(normal_base),
						pays_fee: Pays::Yes,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(4),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(1000, 0).saturating_add(normal_base),
						pays_fee: Pays::No,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(5),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(1000, 0).saturating_add(normal_base),
						pays_fee: Pays::No,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(6),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(500, 0).saturating_add(normal_base),
						pays_fee: Pays::No,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(7),
				event: SysEvent::ExtrinsicFailed {
					dispatch_error: DispatchError::BadOrigin.into(),
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(999, 0).saturating_add(normal_base),
						..Default::default()
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(8),
				event: SysEvent::ExtrinsicFailed {
					dispatch_error: DispatchError::BadOrigin.into(),
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(1000, 0).saturating_add(normal_base),
						pays_fee: Pays::Yes,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(9),
				event: SysEvent::ExtrinsicFailed {
					dispatch_error: DispatchError::BadOrigin.into(),
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(800, 0).saturating_add(normal_base),
						pays_fee: Pays::Yes,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(10),
				event: SysEvent::ExtrinsicFailed {
					dispatch_error: DispatchError::BadOrigin.into(),
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(800, 0).saturating_add(normal_base),
						pays_fee: Pays::No,
						class: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
			EventRecord {
				phase: Phase::ApplyExtrinsic(11),
				event: SysEvent::ExtrinsicSuccess {
					dispatch_info: DispatchEventInfo {
						weight: Weight::from_parts(300, 0).saturating_add(operational_base),
						class: DispatchClass::Operational,
						pays_fee: Default::default(),
					},
				}
				.into(),
				topics: vec![],
			},
		];
		for (i, event) in want.into_iter().enumerate() {
			assert_eq!(got[i], event, "Event mismatch at index {}", i);
		}
	});
}

#[test]
fn deposit_event_topics() {
	new_test_ext().execute_with(|| {
		const BLOCK_NUMBER: u64 = 1;

		System::reset_events();
		System::initialize(&BLOCK_NUMBER, &[0u8; 32].into(), &Default::default());
		System::note_finished_extrinsics();

		let topics = vec![H256::repeat_byte(1), H256::repeat_byte(2), H256::repeat_byte(3)];

		// We deposit a few events with different sets of topics.
		System::deposit_event_indexed(&topics[0..3], SysEvent::NewAccount { account: 1 }.into());
		System::deposit_event_indexed(&topics[0..1], SysEvent::NewAccount { account: 2 }.into());
		System::deposit_event_indexed(&topics[1..2], SysEvent::NewAccount { account: 3 }.into());

		System::finalize();

		// Check that topics are reflected in the event record.
		assert_eq!(
			System::events(),
			vec![
				EventRecord {
					phase: Phase::Finalization,
					event: SysEvent::NewAccount { account: 1 }.into(),
					topics: topics[0..3].to_vec(),
				},
				EventRecord {
					phase: Phase::Finalization,
					event: SysEvent::NewAccount { account: 2 }.into(),
					topics: topics[0..1].to_vec(),
				},
				EventRecord {
					phase: Phase::Finalization,
					event: SysEvent::NewAccount { account: 3 }.into(),
					topics: topics[1..2].to_vec(),
				}
			]
		);

		// Check that the topic-events mapping reflects the deposited topics.
		// Note that these are indexes of the events.
		assert_eq!(System::event_topics(&topics[0]), vec![(BLOCK_NUMBER, 0), (BLOCK_NUMBER, 1)]);
		assert_eq!(System::event_topics(&topics[1]), vec![(BLOCK_NUMBER, 0), (BLOCK_NUMBER, 2)]);
		assert_eq!(System::event_topics(&topics[2]), vec![(BLOCK_NUMBER, 0)]);
	});
}

#[test]
fn event_util_functions_should_work() {
	new_test_ext().execute_with(|| {
		System::set_block_number(1);
		System::deposit_event(SysEvent::CodeUpdated { hash: Default::default() });

		System::assert_has_event(SysEvent::CodeUpdated { hash: Default::default() }.into());
		System::assert_last_event(SysEvent::CodeUpdated { hash: Default::default() }.into());
	});
}

#[test]
fn prunes_block_hash_mappings() {
	new_test_ext().execute_with(|| {
		// simulate import of 15 blocks
		for n in 1..=15 {
			System::reset_events();
			System::initialize(&n, &[n as u8 - 1; 32].into(), &Default::default());

			System::finalize();
		}

		// first 5 block hashes are pruned
		for n in 0..5 {
			assert_eq!(System::block_hash(n), H256::zero());
		}

		// the remaining 10 are kept
		for n in 5..15 {
			assert_eq!(System::block_hash(n), [n as u8; 32].into());
		}
	})
}

#[test]
fn set_code_checks_works() {
	struct ReadRuntimeVersion(Vec<u8>);

	impl sp_core::traits::ReadRuntimeVersion for ReadRuntimeVersion {
		fn read_runtime_version(
			&self,
			_wasm_code: &[u8],
			_ext: &mut dyn sp_externalities::Externalities,
		) -> Result<Vec<u8>, String> {
			Ok(self.0.clone())
		}
	}

	let test_data = vec![
		("test", 1, 2, Err(Error::<Test>::SpecVersionNeedsToIncrease)),
		("test", 1, 1, Err(Error::<Test>::SpecVersionNeedsToIncrease)),
		("test2", 1, 1, Err(Error::<Test>::InvalidSpecName)),
		(
			"test",
			2,
			1,
			Ok(Some(<mock::Test as pallet::Config>::BlockWeights::get().max_block).into()),
		),
		("test", 0, 1, Err(Error::<Test>::SpecVersionNeedsToIncrease)),
		("test", 1, 0, Err(Error::<Test>::SpecVersionNeedsToIncrease)),
	];

	for (spec_name, spec_version, impl_version, expected) in test_data.into_iter() {
		let version = RuntimeVersion {
			spec_name: spec_name.into(),
			spec_version,
			impl_version,
			..Default::default()
		};
		let read_runtime_version = ReadRuntimeVersion(version.encode());

		let mut ext = new_test_ext();
		ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(read_runtime_version));
		ext.execute_with(|| {
			let res = System::set_code(RawOrigin::Root.into(), vec![1, 2, 3, 4]);

			assert_runtime_updated_digest(if res.is_ok() { 1 } else { 0 });
			assert_eq!(expected.map_err(DispatchErrorWithPostInfo::from), res);
		});
	}
}

fn assert_runtime_updated_digest(num: usize) {
	assert_eq!(
		System::digest()
			.logs
			.into_iter()
			.filter(|item| *item == generic::DigestItem::RuntimeEnvironmentUpdated)
			.count(),
		num,
		"Incorrect number of Runtime Updated digest items",
	);
}

#[test]
fn set_code_with_real_wasm_blob() {
	let executor = WasmExecutor::default();
	let mut ext = new_test_ext();
	ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(executor));
	ext.execute_with(|| {
		System::set_block_number(1);
		System::set_code(
			RawOrigin::Root.into(),
			substrate_test_runtime_client::runtime::wasm_binary_unwrap().to_vec(),
		)
		.unwrap();

		let hash = BlakeTwo256::hash(&substrate_test_runtime_client::runtime::wasm_binary_unwrap());
		assert_eq!(
			System::events(),
			vec![EventRecord {
				phase: Phase::Initialization,
				event: SysEvent::CodeUpdated { hash }.into(),
				topics: vec![],
			}],
		);
	});
}

#[test]
fn set_code_rejects_during_mbm() {
	Ongoing::set(true);

	let executor = substrate_test_runtime_client::WasmExecutor::default();
	let mut ext = new_test_ext();
	ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(executor));
	ext.execute_with(|| {
		System::set_block_number(1);
		let res = System::set_code(
			RawOrigin::Root.into(),
			substrate_test_runtime_client::runtime::wasm_binary_unwrap().to_vec(),
		);
		assert_eq!(
			res,
			Err(DispatchErrorWithPostInfo::from(Error::<Test>::MultiBlockMigrationsOngoing))
		);

		assert!(System::events().is_empty());
	});
}

#[test]
fn set_code_via_authorization_works() {
	let executor = substrate_test_runtime_client::WasmExecutor::default();
	let mut ext = new_test_ext();
	ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(executor));
	ext.execute_with(|| {
		System::set_block_number(1);
		assert!(System::authorized_upgrade().is_none());

		let runtime = substrate_test_runtime_client::runtime::wasm_binary_unwrap().to_vec();
		let hash = <mock::Test as pallet::Config>::Hashing::hash(&runtime);

		// Can't apply before authorization
		assert_noop!(
			System::apply_authorized_upgrade(RawOrigin::None.into(), runtime.clone()),
			Error::<Test>::NothingAuthorized,
		);

		// Can authorize
		assert_ok!(System::authorize_upgrade(RawOrigin::Root.into(), hash));
		System::assert_has_event(
			SysEvent::UpgradeAuthorized { code_hash: hash, check_version: true }.into(),
		);
		assert_eq!(System::authorized_upgrade().unwrap().code_hash(), &hash);

		// Can't be sneaky
		let mut bad_runtime = substrate_test_runtime_client::runtime::wasm_binary_unwrap().to_vec();
		bad_runtime.extend(b"sneaky");
		assert_noop!(
			System::apply_authorized_upgrade(RawOrigin::None.into(), bad_runtime),
			Error::<Test>::Unauthorized,
		);

		// Can apply correct runtime
		assert_ok!(System::apply_authorized_upgrade(RawOrigin::None.into(), runtime.to_vec()));
		System::assert_has_event(SysEvent::CodeUpdated { hash }.into());
		assert!(System::authorized_upgrade().is_none());
	});
}

#[test]
fn runtime_upgraded_with_set_storage() {
	let executor = substrate_test_runtime_client::WasmExecutor::default();
	let mut ext = new_test_ext();
	ext.register_extension(sp_core::traits::ReadRuntimeVersionExt::new(executor));
	ext.execute_with(|| {
		System::set_storage(
			RawOrigin::Root.into(),
			vec![(
				well_known_keys::CODE.to_vec(),
				substrate_test_runtime_client::runtime::wasm_binary_unwrap().to_vec(),
			)],
		)
		.unwrap();
	});
}

#[test]
fn events_not_emitted_during_genesis() {
	new_test_ext().execute_with(|| {
		// Block Number is zero at genesis
		assert!(System::block_number().is_zero());
		let mut account_data = AccountInfo::default();
		System::on_created_account(Default::default(), &mut account_data);
		// No events registered at the genesis block
		assert!(!System::read_events_no_consensus().any(|_| true));
		// Events will be emitted starting on block 1
		System::set_block_number(1);
		System::on_created_account(Default::default(), &mut account_data);
		assert!(System::events().len() == 1);
	});
}

#[test]
fn extrinsics_root_is_calculated_correctly() {
	new_test_ext().execute_with(|| {
		System::reset_events();
		System::initialize(&1, &[0u8; 32].into(), &Default::default());
		System::note_finished_initialize();
		System::note_extrinsic(vec![1]);
		System::note_applied_extrinsic(&Ok(().into()), Default::default());
		System::note_extrinsic(vec![2]);
		System::note_applied_extrinsic(&Err(DispatchError::BadOrigin.into()), Default::default());
		System::note_finished_extrinsics();
		let header = System::finalize();

		let ext_root = extrinsics_data_root::<BlakeTwo256>(
			vec![vec![1], vec![2]],
			sp_core::storage::StateVersion::V0,
		);
		assert_eq!(ext_root, *header.extrinsics_root());
	});
}

#[test]
fn runtime_updated_digest_emitted_when_heap_pages_changed() {
	new_test_ext().execute_with(|| {
		System::reset_events();
		System::initialize(&1, &[0u8; 32].into(), &Default::default());
		System::set_heap_pages(RawOrigin::Root.into(), 5).unwrap();
		assert_runtime_updated_digest(1);
	});
}

#[test]
fn ensure_signed_stuff_works() {
	struct Members;
	impl SortedMembers<u64> for Members {
		fn sorted_members() -> Vec<u64> {
			(0..10).collect()
		}
	}

	let signed_origin = RuntimeOrigin::signed(0u64);
	assert_ok!(<EnsureSigned<_> as EnsureOrigin<_>>::try_origin(signed_origin.clone()));
	assert_ok!(<EnsureSignedBy<Members, _> as EnsureOrigin<_>>::try_origin(signed_origin));

	#[cfg(feature = "runtime-benchmarks")]
	{
		let successful_origin: RuntimeOrigin =
			<EnsureSigned<_> as EnsureOrigin<_>>::try_successful_origin()
				.expect("EnsureSigned has no successful origin required for the test");
		assert_ok!(<EnsureSigned<_> as EnsureOrigin<_>>::try_origin(successful_origin));

		let successful_origin: RuntimeOrigin =
			<EnsureSignedBy<Members, _> as EnsureOrigin<_>>::try_successful_origin()
				.expect("EnsureSignedBy has no successful origin required for the test");
		assert_ok!(<EnsureSignedBy<Members, _> as EnsureOrigin<_>>::try_origin(successful_origin));
	}
}

pub fn from_actual_ref_time(ref_time: Option<u64>) -> PostDispatchInfo {
	PostDispatchInfo {
		actual_weight: ref_time.map(|t| Weight::from_all(t)),
		pays_fee: Default::default(),
	}
}

pub fn from_post_weight_info(ref_time: Option<u64>, pays_fee: Pays) -> PostDispatchInfo {
	PostDispatchInfo { actual_weight: ref_time.map(|t| Weight::from_all(t)), pays_fee }
}

#[docify::export]
#[test]
fn last_runtime_upgrade_spec_version_usage() {
	#[allow(dead_code)]
	struct Migration;

	impl OnRuntimeUpgrade for Migration {
		fn on_runtime_upgrade() -> Weight {
			// Ensure to compare the spec version against some static version to prevent applying
			// the same migration multiple times.
			//
			// `1337` here is the spec version of the runtime running on chain. If there is maybe
			// a runtime upgrade in the pipeline of being applied, you should use the spec version
			// of this upgrade.
			if System::last_runtime_upgrade_spec_version() > 1337 {
				return Weight::zero();
			}

			// Do the migration.
			Weight::zero()
		}
	}
}

#[test]
fn test_default_account_nonce() {
	new_test_ext().execute_with(|| {
		System::set_block_number(2);
		assert_eq!(System::account_nonce(&1), 2u64.into());

		System::inc_account_nonce(&1);
		assert_eq!(System::account_nonce(&1), 3u64.into());

		System::set_block_number(5);
		assert_eq!(System::account_nonce(&1), 3u64.into());

		Account::<Test>::remove(&1);
		assert_eq!(System::account_nonce(&1), 5u64.into());
	});
}

#[test]
fn extrinsic_weight_refunded_is_cleaned() {
	new_test_ext().execute_with(|| {
		crate::ExtrinsicWeightReclaimed::<Test>::put(Weight::from_parts(1, 2));
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::from_parts(1, 2));
		System::note_applied_extrinsic(&Ok(().into()), Default::default());
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::zero());

		crate::ExtrinsicWeightReclaimed::<Test>::put(Weight::from_parts(1, 2));
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::from_parts(1, 2));
		System::note_applied_extrinsic(&Err(DispatchError::BadOrigin.into()), Default::default());
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::zero());
	});
}

#[test]
fn reclaim_works() {
	new_test_ext().execute_with(|| {
		let info = DispatchInfo { call_weight: Weight::from_parts(100, 200), ..Default::default() };
		crate::Pallet::<Test>::reclaim_weight(
			&info,
			&PostDispatchInfo {
				actual_weight: Some(Weight::from_parts(50, 100)),
				..Default::default()
			},
		)
		.unwrap();
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::from_parts(50, 100));

		crate::Pallet::<Test>::reclaim_weight(
			&info,
			&PostDispatchInfo {
				actual_weight: Some(Weight::from_parts(25, 200)),
				..Default::default()
			},
		)
		.unwrap();
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::from_parts(75, 100));

		crate::Pallet::<Test>::reclaim_weight(
			&info,
			&PostDispatchInfo {
				actual_weight: Some(Weight::from_parts(300, 50)),
				..Default::default()
			},
		)
		.unwrap();
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::from_parts(75, 150));

		crate::Pallet::<Test>::reclaim_weight(
			&info,
			&PostDispatchInfo {
				actual_weight: Some(Weight::from_parts(300, 300)),
				..Default::default()
			},
		)
		.unwrap();
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::from_parts(75, 150));

		System::note_applied_extrinsic(&Ok(().into()), Default::default());
		assert_eq!(crate::ExtrinsicWeightReclaimed::<Test>::get(), Weight::zero());
	});
}

#[test]
#[should_panic(expected = "Block number must be strictly increasing.")]
fn initialize_block_number_must_be_sequential() {
	new_test_ext().execute_with(|| {
		// Initialize block 1
		System::initialize(&1, &[0u8; 32].into(), &Default::default());
		System::finalize();

		// Try to initialize block 3, skipping block 2 - this should panic
		System::initialize(&3, &[0u8; 32].into(), &Default::default());
	});
}

#[test]
fn set_code_version_3_schedules_and_applies_pending_code() {
	let code = vec![1, 2, 3];
	let mut ext = new_test_ext();
	ext.execute_with(|| {
		let mut version = Version::get();
		version.system_version = 3;
		Version::set(version);
		// Move to block 1
		crate::Pallet::<Test>::set_block_number(1);
		// Schedule new code
		assert_ok!(<() as crate::SetCode<Test>>::set_code(code.clone()));
		// Pending code stored
		let pending = storage::unhashed::get_raw(well_known_keys::PENDING_CODE)
			.expect("Pending code should exist");
		assert_eq!(pending, code.clone());
		let pending_hash = BlakeTwo256::hash(&pending);

		// BlocksTillUpgrade should be set to 2
		assert_eq!(BlocksTillUpgrade::<Test>::get(), Some(2u8));
		// Immediate code not updated
		let current = storage::unhashed::get_raw(well_known_keys::CODE).unwrap_or_default();
		assert_ne!(current, code.clone());
		// RuntimeEnvironmentUpdated digest should already be present
		assert!(System::digest()
			.logs()
			.iter()
			.any(|d| *d == sp_runtime::generic::DigestItem::RuntimeEnvironmentUpdated));
		// CodeUpdated event is emitted immediately when the upgrade is scheduled.
		System::assert_has_event(SysEvent::CodeUpdated { hash: pending_hash }.into());
		System::reset_events();

		// First on_finalize (block N): counter goes 2 -> 1, no apply yet
		crate::Pallet::<Test>::maybe_apply_pending_code_upgrade();
		assert_eq!(BlocksTillUpgrade::<Test>::get(), Some(1u8));
		// Code still not updated
		let current = storage::unhashed::get_raw(well_known_keys::CODE).unwrap_or_default();
		assert_ne!(current, code.clone());
		// Second on_finalize (block N+1): counter goes 1 -> 0, apply
		crate::Pallet::<Test>::maybe_apply_pending_code_upgrade();
		// Code should now be updated
		let updated =
			storage::unhashed::get_raw(well_known_keys::CODE).expect("Code should be updated");
		assert_eq!(updated, code);
		// Pending code should be cleaned up
		assert!(storage::unhashed::get_raw(well_known_keys::PENDING_CODE).is_none());
		// BlocksTillUpgrade should be killed
		assert_eq!(BlocksTillUpgrade::<Test>::get(), None);
		// CodeUpdated event is not emitted again
		assert!(System::events().is_empty());
	});
}

#[test]
fn preinherent_digest_is_preserved() {
	new_test_ext().execute_with(|| {
		let data = vec![42u8; 100];
		let digest = Digest { logs: vec![DigestItem::PreRuntime(*b"test", data.clone())] };

		System::initialize(&1, &[0u8; 32].into(), &digest);

		let stored_digest = <crate::Digest<Test>>::get();
		assert_eq!(stored_digest.logs.len(), 1);

		if let Some(DigestItem::PreRuntime(id, stored_data)) = stored_digest.logs.first() {
			assert_eq!(id, b"test");
			assert_eq!(stored_data, &data);
		} else {
			panic!("Expected PreRuntime digest item");
		}

		let header = System::finalize();
		assert_eq!(header.digest().logs.len(), 1);

		if let Some(DigestItem::PreRuntime(id, header_data)) = header.digest().logs.first() {
			assert_eq!(id, b"test");
			assert_eq!(header_data, &data);
		} else {
			panic!("Expected PreRuntime digest item in finalized header");
		}
	});
}

#[test]
fn block_size_includes_digest_and_header_overhead() {
	new_test_ext().execute_with(|| {
		let data = vec![42u8; 100];
		let digest = Digest { logs: vec![DigestItem::PreRuntime(*b"test", data.clone())] };

		System::initialize(&1, &[0u8; 32].into(), &digest);

		let block_size = System::block_size();

		let digest_size = digest.encoded_size();
		use sp_runtime::traits::{Block as BlockT, Header as HeaderT};
		let empty_header = <<Test as Config>::Block as BlockT>::Header::new(
			1,
			Default::default(),
			Default::default(),
			[0u8; 32].into(),
			Default::default(),
		);
		let empty_header_size = empty_header.encoded_size();
		let expected_overhead = digest_size + empty_header_size;

		assert_eq!(block_size as usize, expected_overhead);
		assert!(block_size > 100);
	});
}

#[test]
fn deposit_log_updates_block_size() {
	new_test_ext().execute_with(|| {
		System::initialize(&1, &[0u8; 32].into(), &Default::default());

		let initial_len = System::block_size();

		let log_data = vec![42u8; 1000];
		let log_item = DigestItem::Other(log_data.clone());
		let log_size = log_item.encoded_size();

		System::deposit_log(log_item);

		let new_len = System::block_size();
		assert_eq!(new_len, initial_len + log_size as u32);
	});
}

#[test]
#[should_panic(expected = "Header size")]
fn inherent_digest_exceeding_max_header_size_panics() {
	new_test_ext().execute_with(|| {
		let max_header_size = RuntimeBlockLength::get().max_header_size();
		let large_data = vec![42u8; max_header_size as usize + 10];
		let digest = Digest { logs: vec![DigestItem::PreRuntime(*b"test", large_data)] };

		System::initialize(&1, &[0u8; 32].into(), &digest);
	});
}
