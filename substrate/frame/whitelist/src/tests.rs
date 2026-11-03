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

// Tests for Whitelist Pallet

use crate::{mock::*, Event};
use codec::Encode;
use frame::{
	testing_prelude::*,
	traits::{QueryPreimage, StorePreimage},
};

fn run_to_block(n: u64) {
	while System::block_number() < n {
		System::set_block_number(System::block_number() + 1);
	}
}

fn events() -> Vec<Event<Test>> {
	let result = System::events()
		.into_iter()
		.map(|r| r.event)
		.filter_map(|e| if let RuntimeEvent::Whitelist(inner) = e { Some(inner) } else { None })
		.collect::<Vec<_>>();

	System::reset_events();

	result
}

#[test]
fn test_whitelist_call_and_remove() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![] });
		let encoded_call = call.encode();
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		assert_noop!(
			Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);

		assert_noop!(
			Whitelist::whitelist_call(RuntimeOrigin::signed(1), call_hash),
			DispatchError::BadOrigin,
		);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert!(Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash),
			crate::Error::<Test>::CallAlreadyWhitelisted,
		);

		assert_noop!(
			Whitelist::remove_whitelisted_call(RuntimeOrigin::signed(1), call_hash),
			DispatchError::BadOrigin,
		);

		assert_ok!(Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash));

		assert!(!Preimage::is_requested(&call_hash));

		assert_noop!(
			Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);
	});
}

#[test]
fn test_whitelist_call_and_execute() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });
		let call_weight = call.get_dispatch_info().call_weight;
		let encoded_call = call.encode();
		let call_encoded_len = encoded_call.len() as u32;
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_encoded_len,
			call_weight
		),);

		assert!(events().iter().any(|event| {
			match event {
				Event::<Test>::DispatchDeferred { call_hash: hash } => hash == &call_hash,
				_ => false,
			}
		}));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::AlreadyDeferred,
		);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert!(Preimage::is_requested(&call_hash));

		// Use signed Origin after dispatch has been defeered
		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::signed(1),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::UnavailablePreImage,
		);

		// Use root after dispatch has been defeered
		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::UnavailablePreImage,
		);

		assert_ok!(Preimage::note(encoded_call.into()));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight - Weight::from_parts(1, 0)
			),
			crate::Error::<Test>::InvalidCallWeightWitness,
		);

		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_encoded_len,
			call_weight
		));

		let post_dispatch_events = events();

		assert!(
			post_dispatch_events.iter().any(|event| {
				matches!(event, Event::<Test>::CallWhitelisted { call_hash: hash } if hash == &call_hash)
			}),
			"Expected CallWhitelisted event"
		);

		assert!(
			post_dispatch_events.iter().any(|event| {
				matches!(
					event,
					Event::<Test>::WhitelistedCallDispatched {
						call_hash: hash,
						result: Ok(_)
					} if hash == &call_hash
				)
			}),
			"Expected WhitelistedCallDispatched with Ok result"
		);

		assert!(!Preimage::is_requested(&call_hash));
	});
}

#[test]
fn test_whitelist_call_and_execute_failing_call() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::Whitelist(crate::Call::dispatch_whitelisted_call {
			call_hash: Default::default(),
			call_encoded_len: Default::default(),
			call_weight_witness: Weight::zero(),
		});
		let call_weight = call.get_dispatch_info().call_weight;
		let encoded_call = call.encode();
		let call_encoded_len = encoded_call.len() as u32;
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert_ok!(Preimage::note(encoded_call.into()));
		assert!(Preimage::is_requested(&call_hash));
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_encoded_len,
			call_weight
		));
		assert!(!Preimage::is_requested(&call_hash));
	});
}

#[test]
fn test_whitelist_call_and_execute_without_note_preimage() {
	new_test_ext().execute_with(|| {
		let call = Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![1] }));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert!(Preimage::is_requested(&call_hash));

		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::root(),
			call.clone()
		));

		assert!(events().iter().any(|event| {
			match event {
				Event::<Test>::WhitelistedCallDispatched { call_hash: hash, result } => {
					hash == &call_hash && result.is_ok()
				},
				_ => false,
			}
		}));

		assert!(!Preimage::is_requested(&call_hash));

		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::root(),
			call.clone()
		));

		// Deferring via `dispatch_whitelisted_call_with_preimage` no longer notes the preimage, so
		// the deferral itself registers no preimage request. The relayer below still executes the
		// call by re-supplying its bytes, not by fetching a stored preimage.
		assert!(!Preimage::is_requested(&call_hash));

		assert!(events().iter().any(|event| {
			match event {
				Event::<Test>::DispatchDeferred { call_hash: hash } => hash == &call_hash,
				_ => false,
			}
		}));

		// The deferred call must be whitelisted before a relayer may execute it.
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::signed(1),
			call
		));

		let emitted_events = events();

		assert!(emitted_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::WhitelistedCallDispatched {
					call_hash: hash,
					result: Ok(PostDispatchInfo { actual_weight: None, pays_fee: Pays::Yes })
				} if hash == &call_hash
			)
		}));

		assert!(emitted_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::DeferredDispatchExecuted { call_hash: hash, who: 1 }
				if hash == &call_hash
			)
		}));

		assert!(!Preimage::is_requested(&call_hash));
	});
}

#[test]
fn test_whitelist_call_and_execute_decode_consumes_all() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::System(frame_system::Call::remark_with_event { remark: vec![1] });
		let call_weight = call.get_dispatch_info().call_weight;
		let mut call = call.encode();
		// Appending something does not make the encoded call invalid.
		// This tests that the decode function consumes all data.
		call.extend(call.clone());
		let call_encoded_len = call.len() as u32;

		let call_hash = <Test as frame_system::Config>::Hashing::hash(&call[..]);

		assert_ok!(Preimage::note(call.into()));
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::root(),
				call_hash,
				call_encoded_len,
				call_weight
			),
			crate::Error::<Test>::UndecodableCall,
		);
	});
}

#[test]
fn test_deferred_dispatch_failed_inner_call() {
	new_test_ext().execute_with(|| {
		// This call requires a signed origin, whitelisting dispatches calls with root.
		let call = RuntimeCall::System(frame_system::Call::remark_with_event { remark: vec![1] });
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);
		let call_len = call.encoded_size() as u32;
		let encoded_call = call.encode();
		let call_weight = call.get_dispatch_info().call_weight;

		run_to_block(1);

		// Defer dispatch (no preimage yet)
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_len,
			call_weight
		));

		assert!(events().iter().any(|event| {
			match event {
				Event::<Test>::DispatchDeferred { call_hash: hash } => hash == &call_hash,
				_ => false,
			}
		}));

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert_ok!(Preimage::note(encoded_call.into()));

		assert!(Preimage::is_requested(&call_hash));

		// Try to dispatch with signed origin
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::signed(1),
			call_hash,
			call_len,
			call_weight,
		));

		let emitted_events = events();

		assert!(emitted_events.iter().any(|event| {
			matches!(event, Event::<Test>::CallWhitelisted { call_hash: hash } if hash == &call_hash)
		}));

		// Dispatches with Root when executing whitelisted calls. However,
		// remark_with_event requires RawOrigin::Signed(_)
		assert!(emitted_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::WhitelistedCallDispatched {
					call_hash: hash,
					result: Err(DispatchErrorWithPostInfo { error: DispatchError::BadOrigin, .. })
				} if hash == &call_hash
			)
		}));

		// Even though the inner call failed, the deferred entry was still consumed by the relayer,
		// so `DeferredDispatchExecuted` is emitted regardless of the inner call's outcome.
		assert!(emitted_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::DeferredDispatchExecuted { call_hash: hash, who: 1 }
				if hash == &call_hash
			)
		}));
		assert!(!Preimage::is_requested(&call_hash));
	});
}

#[test]
fn test_deferred_dispatch_expires_after_block_delay() {
	new_test_ext().execute_with(|| {
		let call = RuntimeCall::System(frame_system::Call::remark { remark: vec![1] });
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);
		let call_len = call.encoded_size() as u32;
		let encoded_call = call.encode();
		let call_weight = call.get_dispatch_info().call_weight;

		run_to_block(1);

		// Defer dispatch (no preimage yet)
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_len,
			call_weight
		));

		assert!(events().iter().any(|event| {
			match event {
				Event::<Test>::DispatchDeferred { call_hash: hash } => hash == &call_hash,
				_ => false,
			}
		}));

		run_to_block(16);

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		assert_ok!(Preimage::note(encoded_call.into()));

		assert!(Preimage::is_requested(&call_hash));

		// Signed origin fails with expired error, still will result in dispatch error because the
		// call can only be dispatched by root.
		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::signed(1),
				call_hash,
				call_len,
				call_weight
			),
			crate::Error::<Test>::DeferredDispatchExpired
		);

		// Same for preimage variant
		assert_noop!(
			Whitelist::dispatch_whitelisted_call_with_preimage(
				RuntimeOrigin::signed(1),
				Box::new(call.clone())
			),
			crate::Error::<Test>::DeferredDispatchExpired
		);

		// Root can still dispatch directly, provided call has been whitelisted
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_len,
			call_weight
		));

		let emitted_events = events();

		assert!(emitted_events.iter().any(|event| {
			matches!(event, Event::<Test>::CallWhitelisted { call_hash: hash } if hash == &call_hash)
		}));

		assert!(emitted_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::WhitelistedCallDispatched {
					call_hash: hash,
					result: Ok(PostDispatchInfo { actual_weight: None, pays_fee: Pays::Yes })
				} if hash == &call_hash
			)
		}));

		assert!(!Preimage::is_requested(&call_hash));
	});
}

#[test]
fn test_deferred_dispatch_with_signed_origin() {
	new_test_ext().execute_with(|| {
		// This is a call that both signed origin and root can call, provided all requirements are
		// met.
		let balance_call = RuntimeCall::Balances(pallet_balances::Call::force_transfer {
			source: 1,
			dest: 2,
			value: 100,
		});

		// Fund source account balance
		assert_ok!(Balances::force_set_balance(RuntimeOrigin::root(), 1, 1000));

		let balance_call_hash = <Test as frame_system::Config>::Hashing::hash_of(&balance_call);
		let balance_call_len = balance_call.encoded_size() as u32;
		let balance_encoded_call = balance_call.encode();
		let balance_call_weight = balance_call.get_dispatch_info().call_weight;

		// Initial caller should be root or assigned Origin
		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::signed(1),
				balance_call_hash,
				balance_call_len,
				balance_call_weight,
			),
			crate::Error::<Test>::DeferredDispatchNotFound
		);

		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			balance_call_hash,
			balance_call_len,
			balance_call_weight,
		));

		assert!(events().iter().any(|event| {
			match event {
				Event::<Test>::DispatchDeferred { call_hash: hash } => hash == &balance_call_hash,
				_ => false,
			}
		}));

		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), balance_call_hash));

		assert_ok!(Preimage::note(balance_encoded_call.into()));

		assert!(Preimage::is_requested(&balance_call_hash));

		// Subsequent call to the same hash can be from any signed origin before the dispatch expiry
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::signed(4),
			balance_call_hash,
			balance_call_len,
			balance_call_weight,
		));

		let post_dispatch_events = events();

		assert!(post_dispatch_events.iter().any(|event| {
			matches!(event, Event::<Test>::CallWhitelisted { call_hash: hash } if hash == &balance_call_hash)
		}));

		assert!(post_dispatch_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::WhitelistedCallDispatched { call_hash: hash, result: Ok(_) }
				if hash == &balance_call_hash
			)
		}));

		assert!(post_dispatch_events.iter().any(|event| {
			matches!(
				event,
				Event::<Test>::DeferredDispatchExecuted { call_hash: hash, who: 4 }
				if hash == &balance_call_hash
			)
		}));

		assert!(!Preimage::is_requested(&balance_call_hash));
	});
}

#[test]
fn remove_deferred_dispatch_works() {
	new_test_ext().execute_with(|| {
		let call =
			Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![3u8; 24] }));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		// Root defers the call; it is never whitelisted.
		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(RuntimeOrigin::root(), call));

		// It cannot be removed until it has expired.
		assert_noop!(
			Whitelist::remove_deferred_dispatch(RuntimeOrigin::signed(1), call_hash),
			crate::Error::<Test>::DeferredDispatchNotExpired
		);

		// Past the expiration window, anyone can permissionlessly clean up the entry, paying no
		// fee.
		run_to_block(System::block_number() + 16);
		let post = Whitelist::remove_deferred_dispatch(RuntimeOrigin::signed(1), call_hash)
			.expect("removal of an expired entry succeeds");
		assert_eq!(post.pays_fee, Pays::No);
		assert!(events().iter().any(|event| matches!(
			event,
			Event::<Test>::DeferredDispatchRemoved { call_hash: hash } if hash == &call_hash
		)));

		// The entry is gone: a second removal finds nothing.
		assert_noop!(
			Whitelist::remove_deferred_dispatch(RuntimeOrigin::signed(1), call_hash),
			crate::Error::<Test>::DeferredDispatchNotFound
		);
	});
}

#[test]
fn relayer_cannot_bypass_unwhitelisting() {
	new_test_ext().execute_with(|| {
		let call =
			Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![2u8; 16] }));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::root(),
			call.clone(),
		));
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert_ok!(Whitelist::remove_whitelisted_call(RuntimeOrigin::root(), call_hash));

		// The whitelist was revoked, so a relayer can no longer execute the still-deferred call.
		assert_noop!(
			Whitelist::dispatch_whitelisted_call_with_preimage(RuntimeOrigin::signed(1), call),
			crate::Error::<Test>::CallIsNotWhitelisted,
		);
	});
}

#[test]
fn relay_cannot_be_replayed() {
	new_test_ext().execute_with(|| {
		let call =
			Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![7u8; 8] }));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		// Defer, whitelist, then relay once — succeeds and consumes the deferred entry.
		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::root(),
			call.clone(),
		));
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		let post = Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::signed(1),
			call.clone(),
		)
		.expect("relay succeeds");
		// The relayer is not charged.
		assert_eq!(post.pays_fee, Pays::No);

		// A second relay of the same hash must fail: the authorized dispatch can't be replayed.
		assert_noop!(
			Whitelist::dispatch_whitelisted_call_with_preimage(RuntimeOrigin::signed(1), call),
			crate::Error::<Test>::DeferredDispatchNotFound,
		);
	});
}

#[test]
fn relay_cannot_be_replayed_without_preimage() {
	new_test_ext().execute_with(|| {
		let call =
			Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![6u8; 8] }));
		let call_weight = call.get_dispatch_info().call_weight;
		let encoded_call = call.encode();
		let call_encoded_len = encoded_call.len() as u32;
		let call_hash = <Test as frame_system::Config>::Hashing::hash(&encoded_call[..]);

		// Defer via the privileged origin, note the preimage, then whitelist.
		assert_ok!(Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::root(),
			call_hash,
			call_encoded_len,
			call_weight,
		));
		assert_ok!(Preimage::note(encoded_call.into()));
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));

		// A signed relayer executes the deferred call once — succeeds and consumes the entry.
		let post = Whitelist::dispatch_whitelisted_call(
			RuntimeOrigin::signed(1),
			call_hash,
			call_encoded_len,
			call_weight,
		)
		.expect("relay succeeds");
		// The relayer is not charged.
		assert_eq!(post.pays_fee, Pays::No);

		// A second relay of the same hash must fail: the authorized dispatch can't be replayed.
		assert_noop!(
			Whitelist::dispatch_whitelisted_call(
				RuntimeOrigin::signed(1),
				call_hash,
				call_encoded_len,
				call_weight,
			),
			crate::Error::<Test>::DeferredDispatchNotFound,
		);
	});
}

#[test]
fn deferred_relay_nets_preimage_request_to_zero() {
	new_test_ext().execute_with(|| {
		let call =
			Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![9u8; 12] }));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		// Deferring does not note or request the preimage.
		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::root(),
			call.clone(),
		));
		assert!(!Preimage::is_requested(&call_hash));

		// Whitelisting adds exactly one request.
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert!(Preimage::is_requested(&call_hash));

		// Relaying the deferred call nets the request back to zero with nothing left behind.
		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(
			RuntimeOrigin::signed(1),
			call,
		));
		assert!(!Preimage::is_requested(&call_hash));
		assert_eq!(pallet_preimage::RequestStatusFor::<Test>::iter().count(), 0);
	});
}

#[test]
fn remove_deferred_dispatch_does_not_unwhitelist() {
	new_test_ext().execute_with(|| {
		let call =
			Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![5u8; 20] }));
		let call_hash = <Test as frame_system::Config>::Hashing::hash_of(&call);

		// Defer and whitelist the call.
		assert_ok!(Whitelist::dispatch_whitelisted_call_with_preimage(RuntimeOrigin::root(), call));
		assert_ok!(Whitelist::whitelist_call(RuntimeOrigin::root(), call_hash));
		assert!(Preimage::is_requested(&call_hash));

		// Clean up the deferred entry once it has expired.
		run_to_block(System::block_number() + 16);
		assert_ok!(Whitelist::remove_deferred_dispatch(RuntimeOrigin::signed(1), call_hash));

		// Removing a deferred entry only drops the deferral — the whitelist and its preimage
		// request are left intact.
		assert!(!crate::DeferredDispatch::<Test>::contains_key(call_hash));
		assert!(crate::WhitelistedCall::<Test>::contains_key(call_hash));
		assert!(Preimage::is_requested(&call_hash));
	});
}
