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

//! Setup helpers shared by the XCM benchmarks of the system parachain runtimes.

use alloc::boxed::Box;
use frame_support::{
	assert_ok,
	traits::{Currency, Get},
};
use frame_system::RawOrigin;
use xcm::latest::{Junction::AccountId32, Location};

/// Sets up and returns the worst-case `(origin, target)` pair for the `AliasOrigin` instruction,
/// for a runtime that resolves aliases through `pallet_xcm::AuthorizedAliasers`: `target`
/// authorizes the maximum number of aliasers, and the returned `origin` is the one registered
/// last, carrying an expiry.
///
/// `pallet_xcm::Pallet::is_authorized_alias` stops at the first matching aliaser, so registering
/// the matching one last is what forces the whole list to be iterated; the expiry makes the
/// expiration check run as well.
///
/// The pair is only the worst case if all of the following hold for the calling runtime. They do
/// for every in-tree system parachain, but none of them is checked here — a runtime configured
/// differently has to work out its own worst case:
///
/// - `AuthorizedAliasers` is the *last* entry of `xcm_executor::Config::Aliasers`, so it is reached
///   only once every cheaper filter has been tried and has failed.
/// - None of those cheaper filters matches a pair of two unrelated local accounts. That is the case
///   for `AliasChildLocation`, `AliasAccountId32FromSiblingSystemChain` and
///   `AliasOriginRootUsingFilter`, which the system parachains put ahead of it.
/// - `pallet_xcm::Config::ExecuteXcmOrigin` converts a signed origin into `Location::new(0,
///   [AccountId32 { .. }])`, as `xcm_builder::SignedToAccountId32` does.
///
/// Panics if the setup fails, rather than returning an error, because
/// `pallet_xcm_benchmarks::generic::Config::alias_origin` maps any error to
/// `BenchmarkError::Skip`, which would silently leave the instruction unmeasured.
pub fn set_up_worst_case_authorized_alias<Runtime>() -> (Location, Location)
where
	Runtime: pallet_xcm::Config + pallet_balances::Config,
	<Runtime as frame_system::Config>::AccountId: From<[u8; 32]>,
{
	// `target` is the account authorizing the aliasers. It has to be a local account for
	// `add_authorized_alias` to accept it as the authorizing origin.
	let target_id = [42u8; 32];
	let target_account: <Runtime as frame_system::Config>::AccountId = target_id.into();
	let target = Location::new(0, [AccountId32 { id: target_id, network: None }]);

	// Fund `target` so that it can pay the deposit held for each authorized alias.
	let balance =
		<Runtime as pallet_balances::Config>::ExistentialDeposit::get() * 1_000_000u32.into();
	let _ = <pallet_balances::Pallet<Runtime> as Currency<_>>::make_free_balance_be(
		&target_account,
		balance,
	);
	let target_origin: <Runtime as frame_system::Config>::RuntimeOrigin =
		RawOrigin::Signed(target_account).into();

	// `origin` is the aliaser that ends up matching: a local account distinct from `target`, so
	// that the pair is not matched by any of the cheaper filters.
	let origin = Location::new(0, [AccountId32 { id: [170u8; 32], network: None }]);

	// Fill every authorization slot but one with distinct dummy aliasers...
	for index in 1..pallet_xcm::MaxAuthorizedAliases::get() {
		let mut id = [0u8; 32];
		id[..4].copy_from_slice(&index.to_le_bytes());
		let filler = Location::new(0, [AccountId32 { id, network: None }]);
		assert_ok!(pallet_xcm::Pallet::<Runtime>::add_authorized_alias(
			target_origin.clone(),
			Box::new(filler.into()),
			None,
		));
	}
	// ...and register the matching one last, so that the lookup iterates the whole list.
	assert_ok!(pallet_xcm::Pallet::<Runtime>::add_authorized_alias(
		target_origin,
		Box::new(origin.clone().into()),
		Some(u64::MAX),
	));

	(origin, target)
}
