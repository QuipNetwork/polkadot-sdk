// This file is part of Substrate.

// Copyright (Criterion) Parity Technologies (UK) Ltd.
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

//! Type to combine two `fungibles::*` implementations into one union `fungibles::*` implementation.
//!
//! See the [`crate::traits::fungibles`] doc for more information about fungibles traits.

use frame_support::traits::{
	tokens::{
		fungibles, fungibles::imbalance, AssetId, DepositConsequence, Fortitude, Precision,
		Preservation, Provenance, Restriction, WithdrawConsequence,
	},
	AccountTouch,
};
use sp_runtime::{
	traits::Convert,
	DispatchError, DispatchResult, Either,
	Either::{Left, Right},
};

/// Type to combine two `fungibles::*` implementations into one union `fungibles::*` implementation.
///
/// ### Parameters:
/// - `Left` is `fungibles::*` implementation that is incorporated into the resulting union.
/// - `Right` is `fungibles::*` implementation that is incorporated into the resulting union.
/// - `Criterion` determines whether the `AssetKind` belongs to the `Left` or `Right` set.
/// - `AssetKind` is a superset type encompassing asset kinds from `Left` and `Right` sets.
/// - `AccountId` is an account identifier type.
pub struct UnionOf<Left, Right, Criterion, AssetKind, AccountId>(
	core::marker::PhantomData<(Left, Right, Criterion, AssetKind, AccountId)>,
);

impl<
		Left: fungibles::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Inspect<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type AssetId = AssetKind;
	type Balance = Left::Balance;

	fn total_issuance(asset: Self::AssetId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::total_issuance(a),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::total_issuance(a),
		}
	}
	fn active_issuance(asset: Self::AssetId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::active_issuance(a),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::active_issuance(a),
		}
	}
	fn minimum_balance(asset: Self::AssetId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::minimum_balance(a),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::minimum_balance(a),
		}
	}
	fn balance(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::balance(a, who),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::balance(a, who),
		}
	}
	fn total_balance(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::total_balance(a, who),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::total_balance(a, who),
		}
	}
	fn reducible_balance(
		asset: Self::AssetId,
		who: &AccountId,
		preservation: Preservation,
		force: Fortitude,
	) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::reducible_balance(
				a,
				who,
				preservation,
				force,
			),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::reducible_balance(
				a,
				who,
				preservation,
				force,
			),
		}
	}
	fn can_deposit(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
		provenance: Provenance,
	) -> DepositConsequence {
		match Criterion::convert(asset) {
			Left(a) => {
				<Left as fungibles::Inspect<AccountId>>::can_deposit(a, who, amount, provenance)
			},
			Right(a) => {
				<Right as fungibles::Inspect<AccountId>>::can_deposit(a, who, amount, provenance)
			},
		}
	}
	fn can_withdraw(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
	) -> WithdrawConsequence<Self::Balance> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::can_withdraw(a, who, amount),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::can_withdraw(a, who, amount),
		}
	}
	fn asset_exists(asset: Self::AssetId) -> bool {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Inspect<AccountId>>::asset_exists(a),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::asset_exists(a),
		}
	}
}

impl<
		Left: fungibles::Inspect<AccountId> + fungibles::metadata::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>
			+ fungibles::metadata::Inspect<AccountId>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::metadata::Inspect<AccountId>
	for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn name(asset: Self::AssetId) -> alloc::vec::Vec<u8> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::metadata::Inspect<AccountId>>::name(a),
			Right(a) => <Right as fungibles::metadata::Inspect<AccountId>>::name(a),
		}
	}
	fn symbol(asset: Self::AssetId) -> alloc::vec::Vec<u8> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::metadata::Inspect<AccountId>>::symbol(a),
			Right(a) => <Right as fungibles::metadata::Inspect<AccountId>>::symbol(a),
		}
	}
	fn decimals(asset: Self::AssetId) -> u8 {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::metadata::Inspect<AccountId>>::decimals(a),
			Right(a) => <Right as fungibles::metadata::Inspect<AccountId>>::decimals(a),
		}
	}
}

impl<
		Left: fungibles::Inspect<AccountId> + fungibles::roles::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>
			+ fungibles::roles::Inspect<AccountId>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::roles::Inspect<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn owner(asset: Self::AssetId) -> Option<AccountId> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::roles::Inspect<AccountId>>::owner(a),
			Right(a) => <Right as fungibles::roles::Inspect<AccountId>>::owner(a),
		}
	}
	fn issuer(asset: Self::AssetId) -> Option<AccountId> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::roles::Inspect<AccountId>>::issuer(a),
			Right(a) => <Right as fungibles::roles::Inspect<AccountId>>::issuer(a),
		}
	}
	fn admin(asset: Self::AssetId) -> Option<AccountId> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::roles::Inspect<AccountId>>::admin(a),
			Right(a) => <Right as fungibles::roles::Inspect<AccountId>>::admin(a),
		}
	}
	fn freezer(asset: Self::AssetId) -> Option<AccountId> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::roles::Inspect<AccountId>>::freezer(a),
			Right(a) => <Right as fungibles::roles::Inspect<AccountId>>::freezer(a),
		}
	}
}

impl<
		Left: fungibles::Inspect<AccountId>
			+ fungibles::metadata::Inspect<AccountId>
			+ fungibles::metadata::Mutate<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>
			+ fungibles::metadata::Inspect<AccountId>
			+ fungibles::metadata::Mutate<AccountId>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::metadata::Mutate<AccountId>
	for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn set(
		asset: Self::AssetId,
		from: &AccountId,
		name: alloc::vec::Vec<u8>,
		symbol: alloc::vec::Vec<u8>,
		decimals: u8,
	) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::metadata::Mutate<AccountId>>::set(
				a, from, name, symbol, decimals,
			),
			Right(a) => <Right as fungibles::metadata::Mutate<AccountId>>::set(
				a, from, name, symbol, decimals,
			),
		}
	}
}

impl<
		Left: fungibles::InspectHold<AccountId>,
		Right: fungibles::InspectHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::InspectHold<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type Reason = Left::Reason;

	fn reducible_total_balance_on_hold(
		asset: Self::AssetId,
		who: &AccountId,
		force: Fortitude,
	) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => {
				<Left as fungibles::InspectHold<AccountId>>::reducible_total_balance_on_hold(
					a, who, force,
				)
			},
			Right(a) => {
				<Right as fungibles::InspectHold<AccountId>>::reducible_total_balance_on_hold(
					a, who, force,
				)
			},
		}
	}
	fn hold_available(asset: Self::AssetId, reason: &Self::Reason, who: &AccountId) -> bool {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::InspectHold<AccountId>>::hold_available(a, reason, who),
			Right(a) => {
				<Right as fungibles::InspectHold<AccountId>>::hold_available(a, reason, who)
			},
		}
	}
	fn total_balance_on_hold(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::InspectHold<AccountId>>::total_balance_on_hold(a, who),
			Right(a) => <Right as fungibles::InspectHold<AccountId>>::total_balance_on_hold(a, who),
		}
	}
	fn balance_on_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
	) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::InspectHold<AccountId>>::balance_on_hold(a, reason, who),
			Right(a) => {
				<Right as fungibles::InspectHold<AccountId>>::balance_on_hold(a, reason, who)
			},
		}
	}
	fn can_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
	) -> bool {
		match Criterion::convert(asset) {
			Left(a) => {
				<Left as fungibles::InspectHold<AccountId>>::can_hold(a, reason, who, amount)
			},
			Right(a) => {
				<Right as fungibles::InspectHold<AccountId>>::can_hold(a, reason, who, amount)
			},
		}
	}
}

impl<
		Left: fungibles::InspectFreeze<AccountId>,
		Right: fungibles::InspectFreeze<AccountId, Balance = Left::Balance, Id = Left::Id>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::InspectFreeze<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type Id = Left::Id;
	fn balance_frozen(asset: Self::AssetId, id: &Self::Id, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::InspectFreeze<AccountId>>::balance_frozen(a, id, who),
			Right(a) => <Right as fungibles::InspectFreeze<AccountId>>::balance_frozen(a, id, who),
		}
	}
	fn balance_freezable(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::InspectFreeze<AccountId>>::balance_freezable(a, who),
			Right(a) => <Right as fungibles::InspectFreeze<AccountId>>::balance_freezable(a, who),
		}
	}
	fn can_freeze(asset: Self::AssetId, id: &Self::Id, who: &AccountId) -> bool {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::InspectFreeze<AccountId>>::can_freeze(a, id, who),
			Right(a) => <Right as fungibles::InspectFreeze<AccountId>>::can_freeze(a, id, who),
		}
	}
}

impl<
		Left: fungibles::Unbalanced<AccountId>,
		Right: fungibles::Unbalanced<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Unbalanced<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn handle_dust(dust: fungibles::Dust<AccountId, Self>)
	where
		Self: Sized,
	{
		match Criterion::convert(dust.0) {
			Left(a) => {
				<Left as fungibles::Unbalanced<AccountId>>::handle_dust(fungibles::Dust(a, dust.1))
			},
			Right(a) => {
				<Right as fungibles::Unbalanced<AccountId>>::handle_dust(fungibles::Dust(a, dust.1))
			},
		}
	}
	fn write_balance(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
	) -> Result<Option<Self::Balance>, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Unbalanced<AccountId>>::write_balance(a, who, amount),
			Right(a) => <Right as fungibles::Unbalanced<AccountId>>::write_balance(a, who, amount),
		}
	}
	fn set_total_issuance(asset: Self::AssetId, amount: Self::Balance) -> () {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Unbalanced<AccountId>>::set_total_issuance(a, amount),
			Right(a) => <Right as fungibles::Unbalanced<AccountId>>::set_total_issuance(a, amount),
		}
	}
	fn decrease_balance(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		preservation: Preservation,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Unbalanced<AccountId>>::decrease_balance(
				a,
				who,
				amount,
				precision,
				preservation,
				force,
			),
			Right(a) => <Right as fungibles::Unbalanced<AccountId>>::decrease_balance(
				a,
				who,
				amount,
				precision,
				preservation,
				force,
			),
		}
	}
	fn increase_balance(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Unbalanced<AccountId>>::increase_balance(
				a, who, amount, precision,
			),
			Right(a) => <Right as fungibles::Unbalanced<AccountId>>::increase_balance(
				a, who, amount, precision,
			),
		}
	}
}

impl<
		Left: fungibles::UnbalancedHold<AccountId>,
		Right: fungibles::UnbalancedHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::UnbalancedHold<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn set_balance_on_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
	) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::UnbalancedHold<AccountId>>::set_balance_on_hold(
				a, reason, who, amount,
			),
			Right(a) => <Right as fungibles::UnbalancedHold<AccountId>>::set_balance_on_hold(
				a, reason, who, amount,
			),
		}
	}
	fn decrease_balance_on_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::UnbalancedHold<AccountId>>::decrease_balance_on_hold(
				a, reason, who, amount, precision,
			),
			Right(a) => <Right as fungibles::UnbalancedHold<AccountId>>::decrease_balance_on_hold(
				a, reason, who, amount, precision,
			),
		}
	}
	fn increase_balance_on_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::UnbalancedHold<AccountId>>::increase_balance_on_hold(
				a, reason, who, amount, precision,
			),
			Right(a) => <Right as fungibles::UnbalancedHold<AccountId>>::increase_balance_on_hold(
				a, reason, who, amount, precision,
			),
		}
	}
}

impl<
		Left: fungibles::Mutate<AccountId>,
		Right: fungibles::Mutate<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId: Eq,
	> fungibles::Mutate<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn mint_into(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Mutate<AccountId>>::mint_into(a, who, amount),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::mint_into(a, who, amount),
		}
	}
	fn burn_from(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
		preservation: Preservation,
		precision: Precision,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Mutate<AccountId>>::burn_from(
				a,
				who,
				amount,
				preservation,
				precision,
				force,
			),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::burn_from(
				a,
				who,
				amount,
				preservation,
				precision,
				force,
			),
		}
	}
	fn shelve(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Mutate<AccountId>>::shelve(a, who, amount),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::shelve(a, who, amount),
		}
	}
	fn restore(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Mutate<AccountId>>::restore(a, who, amount),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::restore(a, who, amount),
		}
	}
	fn transfer(
		asset: Self::AssetId,
		source: &AccountId,
		dest: &AccountId,
		amount: Self::Balance,
		preservation: Preservation,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Mutate<AccountId>>::transfer(
				a,
				source,
				dest,
				amount,
				preservation,
			),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::transfer(
				a,
				source,
				dest,
				amount,
				preservation,
			),
		}
	}

	fn set_balance(asset: Self::AssetId, who: &AccountId, amount: Self::Balance) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Mutate<AccountId>>::set_balance(a, who, amount),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::set_balance(a, who, amount),
		}
	}
}

impl<
		Left: fungibles::MutateHold<AccountId>,
		Right: fungibles::MutateHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::MutateHold<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
	) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateHold<AccountId>>::hold(a, reason, who, amount),
			Right(a) => <Right as fungibles::MutateHold<AccountId>>::hold(a, reason, who, amount),
		}
	}
	fn release(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateHold<AccountId>>::release(
				a, reason, who, amount, precision,
			),
			Right(a) => <Right as fungibles::MutateHold<AccountId>>::release(
				a, reason, who, amount, precision,
			),
		}
	}
	fn burn_held(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateHold<AccountId>>::burn_held(
				a, reason, who, amount, precision, force,
			),
			Right(a) => <Right as fungibles::MutateHold<AccountId>>::burn_held(
				a, reason, who, amount, precision, force,
			),
		}
	}
	fn transfer_on_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		source: &AccountId,
		dest: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		mode: Restriction,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateHold<AccountId>>::transfer_on_hold(
				a, reason, source, dest, amount, precision, mode, force,
			),
			Right(a) => <Right as fungibles::MutateHold<AccountId>>::transfer_on_hold(
				a, reason, source, dest, amount, precision, mode, force,
			),
		}
	}
	fn transfer_and_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		source: &AccountId,
		dest: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		preservation: Preservation,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateHold<AccountId>>::transfer_and_hold(
				a,
				reason,
				source,
				dest,
				amount,
				precision,
				preservation,
				force,
			),
			Right(a) => <Right as fungibles::MutateHold<AccountId>>::transfer_and_hold(
				a,
				reason,
				source,
				dest,
				amount,
				precision,
				preservation,
				force,
			),
		}
	}
}

impl<
		Left: fungibles::MutateFreeze<AccountId>,
		Right: fungibles::MutateFreeze<AccountId, Balance = Left::Balance, Id = Left::Id>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::MutateFreeze<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn set_freeze(
		asset: Self::AssetId,
		id: &Self::Id,
		who: &AccountId,
		amount: Self::Balance,
	) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateFreeze<AccountId>>::set_freeze(a, id, who, amount),
			Right(a) => {
				<Right as fungibles::MutateFreeze<AccountId>>::set_freeze(a, id, who, amount)
			},
		}
	}
	fn extend_freeze(
		asset: Self::AssetId,
		id: &Self::Id,
		who: &AccountId,
		amount: Self::Balance,
	) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => {
				<Left as fungibles::MutateFreeze<AccountId>>::extend_freeze(a, id, who, amount)
			},
			Right(a) => {
				<Right as fungibles::MutateFreeze<AccountId>>::extend_freeze(a, id, who, amount)
			},
		}
	}
	fn thaw(asset: Self::AssetId, id: &Self::Id, who: &AccountId) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::MutateFreeze<AccountId>>::thaw(a, id, who),
			Right(a) => <Right as fungibles::MutateFreeze<AccountId>>::thaw(a, id, who),
		}
	}
}

pub struct ConvertImbalanceDropHandler<
	Left,
	Right,
	LeftAssetId,
	RightAssetId,
	Criterion,
	AssetKind,
	Balance,
	AccountId,
>(
	core::marker::PhantomData<(
		Left,
		Right,
		LeftAssetId,
		RightAssetId,
		Criterion,
		AssetKind,
		Balance,
		AccountId,
	)>,
);

impl<
		Left: fungibles::HandleImbalanceDrop<LeftAssetId, Balance>,
		Right: fungibles::HandleImbalanceDrop<RightAssetId, Balance>,
		LeftAssetId,
		RightAssetId,
		Criterion: Convert<AssetKind, Either<LeftAssetId, RightAssetId>>,
		AssetKind,
		Balance,
		AccountId,
	> fungibles::HandleImbalanceDrop<AssetKind, Balance>
	for ConvertImbalanceDropHandler<
		Left,
		Right,
		LeftAssetId,
		RightAssetId,
		Criterion,
		AssetKind,
		Balance,
		AccountId,
	>
{
	fn handle(asset: AssetKind, amount: Balance) {
		match Criterion::convert(asset) {
			Left(a) => Left::handle(a, amount),
			Right(a) => Right::handle(a, amount),
		}
	}
}

impl<
		Left: fungibles::Balanced<AccountId>,
		Right: fungibles::Balanced<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Balanced<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type OnDropDebt = ConvertImbalanceDropHandler<
		Left::OnDropDebt,
		Right::OnDropDebt,
		Left::AssetId,
		Right::AssetId,
		Criterion,
		AssetKind,
		Left::Balance,
		AccountId,
	>;
	type OnDropCredit = ConvertImbalanceDropHandler<
		Left::OnDropCredit,
		Right::OnDropCredit,
		Left::AssetId,
		Right::AssetId,
		Criterion,
		AssetKind,
		Left::Balance,
		AccountId,
	>;

	fn deposit(
		asset: Self::AssetId,
		who: &AccountId,
		value: Self::Balance,
		precision: Precision,
	) -> Result<fungibles::Debt<AccountId, Self>, DispatchError> {
		match Criterion::convert(asset.clone()) {
			Left(a) => <Left as fungibles::Balanced<AccountId>>::deposit(a, who, value, precision)
				.map(|debt| imbalance::from_fungibles(debt, asset)),
			Right(a) => {
				<Right as fungibles::Balanced<AccountId>>::deposit(a, who, value, precision)
					.map(|debt| imbalance::from_fungibles(debt, asset))
			},
		}
	}
	fn issue(asset: Self::AssetId, amount: Self::Balance) -> fungibles::Credit<AccountId, Self> {
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				let credit = <Left as fungibles::Balanced<AccountId>>::issue(a, amount);
				imbalance::from_fungibles(credit, asset)
			},
			Right(a) => {
				let credit = <Right as fungibles::Balanced<AccountId>>::issue(a, amount);
				imbalance::from_fungibles(credit, asset)
			},
		}
	}
	fn pair(
		asset: Self::AssetId,
		amount: Self::Balance,
	) -> Result<(fungibles::Debt<AccountId, Self>, fungibles::Credit<AccountId, Self>), DispatchError>
	{
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				let (a, b) = <Left as fungibles::Balanced<AccountId>>::pair(a, amount)?;
				Ok((
					imbalance::from_fungibles(a, asset.clone()),
					imbalance::from_fungibles(b, asset),
				))
			},
			Right(a) => {
				let (a, b) = <Right as fungibles::Balanced<AccountId>>::pair(a, amount)?;
				Ok((
					imbalance::from_fungibles(a, asset.clone()),
					imbalance::from_fungibles(b, asset),
				))
			},
		}
	}
	fn rescind(asset: Self::AssetId, amount: Self::Balance) -> fungibles::Debt<AccountId, Self> {
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				let debt = <Left as fungibles::Balanced<AccountId>>::rescind(a, amount);
				imbalance::from_fungibles(debt, asset)
			},
			Right(a) => {
				let debt = <Right as fungibles::Balanced<AccountId>>::rescind(a, amount);
				imbalance::from_fungibles(debt, asset)
			},
		}
	}
	fn resolve(
		who: &AccountId,
		credit: fungibles::Credit<AccountId, Self>,
	) -> Result<(), fungibles::Credit<AccountId, Self>> {
		let asset = credit.asset();
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				let credit = imbalance::from_fungibles(credit, a);
				<Left as fungibles::Balanced<AccountId>>::resolve(who, credit)
					.map_err(|credit| imbalance::from_fungibles(credit, asset))
			},
			Right(a) => {
				let credit = imbalance::from_fungibles(credit, a);
				<Right as fungibles::Balanced<AccountId>>::resolve(who, credit)
					.map_err(|credit| imbalance::from_fungibles(credit, asset))
			},
		}
	}
	fn settle(
		who: &AccountId,
		debt: fungibles::Debt<AccountId, Self>,
		preservation: Preservation,
	) -> Result<fungibles::Credit<AccountId, Self>, fungibles::Debt<AccountId, Self>> {
		let asset = debt.asset();
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				let debt = imbalance::from_fungibles(debt, a);
				match <Left as fungibles::Balanced<AccountId>>::settle(who, debt, preservation) {
					Ok(credit) => Ok(imbalance::from_fungibles(credit, asset)),
					Err(debt) => Err(imbalance::from_fungibles(debt, asset)),
				}
			},
			Right(a) => {
				let debt = imbalance::from_fungibles(debt, a);
				match <Right as fungibles::Balanced<AccountId>>::settle(who, debt, preservation) {
					Ok(credit) => Ok(imbalance::from_fungibles(credit, asset)),
					Err(debt) => Err(imbalance::from_fungibles(debt, asset)),
				}
			},
		}
	}
	fn withdraw(
		asset: Self::AssetId,
		who: &AccountId,
		value: Self::Balance,
		precision: Precision,
		preservation: Preservation,
		force: Fortitude,
	) -> Result<fungibles::Credit<AccountId, Self>, DispatchError> {
		match Criterion::convert(asset.clone()) {
			Left(a) => <Left as fungibles::Balanced<AccountId>>::withdraw(
				a,
				who,
				value,
				precision,
				preservation,
				force,
			)
			.map(|credit| imbalance::from_fungibles(credit, asset)),
			Right(a) => <Right as fungibles::Balanced<AccountId>>::withdraw(
				a,
				who,
				value,
				precision,
				preservation,
				force,
			)
			.map(|credit| imbalance::from_fungibles(credit, asset)),
		}
	}
}

impl<
		Left: fungibles::BalancedHold<AccountId>
			+ fungibles::hold::DoneSlash<Self::AssetId, Self::Reason, AccountId, Self::Balance>,
		Right: fungibles::BalancedHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>
			+ fungibles::hold::DoneSlash<Self::AssetId, Self::Reason, AccountId, Self::Balance>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::BalancedHold<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn slash(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
	) -> (fungibles::Credit<AccountId, Self>, Self::Balance) {
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				let (credit, amount) =
					<Left as fungibles::BalancedHold<AccountId>>::slash(a, reason, who, amount);
				(imbalance::from_fungibles(credit, asset), amount)
			},
			Right(a) => {
				let (credit, amount) =
					<Right as fungibles::BalancedHold<AccountId>>::slash(a, reason, who, amount);
				(imbalance::from_fungibles(credit, asset), amount)
			},
		}
	}
}

impl<
		Reason,
		Balance,
		Left: fungibles::Inspect<AccountId>
			+ fungibles::hold::DoneSlash<Left::AssetId, Reason, AccountId, Balance>,
		Right: fungibles::Inspect<AccountId>
			+ fungibles::hold::DoneSlash<Right::AssetId, Reason, AccountId, Balance>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::hold::DoneSlash<AssetKind, Reason, AccountId, Balance>
	for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn done_slash(asset: AssetKind, reason: &Reason, who: &AccountId, amount: Balance) {
		match Criterion::convert(asset.clone()) {
			Left(a) => {
				Left::done_slash(a, reason, who, amount);
			},
			Right(a) => {
				Right::done_slash(a, reason, who, amount);
			},
		}
	}
}

impl<
		Left: fungibles::Inspect<AccountId> + fungibles::Create<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance> + fungibles::Create<AccountId>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Create<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn create(
		asset: AssetKind,
		admin: AccountId,
		is_sufficient: bool,
		min_balance: Self::Balance,
	) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => {
				<Left as fungibles::Create<AccountId>>::create(a, admin, is_sufficient, min_balance)
			},
			Right(a) => <Right as fungibles::Create<AccountId>>::create(
				a,
				admin,
				is_sufficient,
				min_balance,
			),
		}
	}
}

impl<
		Left: fungibles::Inspect<AccountId> + AccountTouch<Left::AssetId, AccountId>,
		Right: fungibles::Inspect<AccountId>
			+ AccountTouch<
				Right::AssetId,
				AccountId,
				Balance = <Left as AccountTouch<Left::AssetId, AccountId>>::Balance,
			>,
		Criterion: Convert<AssetKind, Either<Left::AssetId, Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> AccountTouch<AssetKind, AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type Balance = <Left as AccountTouch<Left::AssetId, AccountId>>::Balance;

	fn deposit_required(asset: AssetKind) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(a) => <Left as AccountTouch<Left::AssetId, AccountId>>::deposit_required(a),
			Right(a) => <Right as AccountTouch<Right::AssetId, AccountId>>::deposit_required(a),
		}
	}

	fn should_touch(asset: AssetKind, who: &AccountId) -> bool {
		match Criterion::convert(asset) {
			Left(a) => <Left as AccountTouch<Left::AssetId, AccountId>>::should_touch(a, who),
			Right(a) => <Right as AccountTouch<Right::AssetId, AccountId>>::should_touch(a, who),
		}
	}

	fn touch(asset: AssetKind, who: &AccountId, depositor: &AccountId) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as AccountTouch<Left::AssetId, AccountId>>::touch(a, who, depositor),
			Right(a) => {
				<Right as AccountTouch<Right::AssetId, AccountId>>::touch(a, who, depositor)
			},
		}
	}
}

impl<
		Left: fungibles::Inspect<AccountId> + fungibles::Refund<AccountId>,
		Right: fungibles::Inspect<AccountId>
			+ fungibles::Refund<AccountId, Balance = <Left as fungibles::Refund<AccountId>>::Balance>,
		Criterion: Convert<
			AssetKind,
			Either<
				<Left as fungibles::Refund<AccountId>>::AssetId,
				<Right as fungibles::Refund<AccountId>>::AssetId,
			>,
		>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Refund<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type AssetId = AssetKind;
	type Balance = <Left as fungibles::Refund<AccountId>>::Balance;

	fn deposit_held(asset: AssetKind, who: AccountId) -> Option<(AccountId, Self::Balance)> {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Refund<AccountId>>::deposit_held(a, who),
			Right(a) => <Right as fungibles::Refund<AccountId>>::deposit_held(a, who),
		}
	}
	fn refund(asset: AssetKind, who: AccountId) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(a) => <Left as fungibles::Refund<AccountId>>::refund(a, who),
			Right(a) => <Right as fungibles::Refund<AccountId>>::refund(a, who),
		}
	}
}
