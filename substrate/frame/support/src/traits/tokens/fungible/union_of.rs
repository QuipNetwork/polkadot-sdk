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

//! Types to combine some `fungible::*` and `fungibles::*` implementations into one union
//! `fungibles::*` implementation.
//!
//! See the [`crate::traits::fungible`] doc for more information about fungible traits.

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::cmp::Ordering;
use frame_support::traits::{
	fungible::imbalance,
	tokens::{
		fungible, fungibles, AssetId, DepositConsequence, Fortitude, Precision, Preservation,
		Provenance, Restriction, WithdrawConsequence,
	},
	AccountTouch,
};
use scale_info::TypeInfo;
use sp_runtime::{
	traits::Convert,
	Debug, DispatchError, DispatchResult, Either,
	Either::{Left, Right},
};

/// The `NativeOrWithId` enum classifies an asset as either `Native` to the current chain or as an
/// asset with a specific ID.
#[derive(
	Decode,
	DecodeWithMemTracking,
	Encode,
	Default,
	MaxEncodedLen,
	TypeInfo,
	Clone,
	Debug,
	Eq,
	serde::Serialize,
	serde::Deserialize,
)]
pub enum NativeOrWithId<AssetId>
where
	AssetId: Ord,
{
	/// Represents the native asset of the current chain.
	///
	/// E.g., DOT for the Polkadot Asset Hub.
	#[default]
	Native,
	/// Represents an asset identified by its underlying `AssetId`.
	WithId(AssetId),
}
impl<AssetId: Ord> From<AssetId> for NativeOrWithId<AssetId> {
	fn from(asset: AssetId) -> Self {
		Self::WithId(asset)
	}
}
impl<AssetId: Ord> Ord for NativeOrWithId<AssetId> {
	fn cmp(&self, other: &Self) -> Ordering {
		match (self, other) {
			(Self::Native, Self::Native) => Ordering::Equal,
			(Self::Native, Self::WithId(_)) => Ordering::Less,
			(Self::WithId(_), Self::Native) => Ordering::Greater,
			(Self::WithId(id1), Self::WithId(id2)) => <AssetId as Ord>::cmp(id1, id2),
		}
	}
}
impl<AssetId: Ord> PartialOrd for NativeOrWithId<AssetId> {
	fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
		Some(<Self as Ord>::cmp(self, other))
	}
}
impl<AssetId: Ord> PartialEq for NativeOrWithId<AssetId> {
	fn eq(&self, other: &Self) -> bool {
		self.cmp(other) == Ordering::Equal
	}
}

/// Criterion for [`UnionOf`] where a set for [`NativeOrWithId::Native`] asset located from the left
/// and for [`NativeOrWithId::WithId`] from the right.
pub struct NativeFromLeft;
impl<AssetId: Ord> Convert<NativeOrWithId<AssetId>, Either<(), AssetId>> for NativeFromLeft {
	fn convert(asset: NativeOrWithId<AssetId>) -> Either<(), AssetId> {
		match asset {
			NativeOrWithId::Native => Either::Left(()),
			NativeOrWithId::WithId(id) => Either::Right(id),
		}
	}
}

/// Type to combine some `fungible::*` and `fungibles::*` implementations into one union
/// `fungibles::*` implementation.
///
/// ### Parameters:
/// - `Left` is `fungible::*` implementation that is incorporated into the resulting union.
/// - `Right` is `fungibles::*` implementation that is incorporated into the resulting union.
/// - `Criterion` determines whether the `AssetKind` belongs to the `Left` or `Right` set.
/// - `AssetKind` is a superset type encompassing asset kinds from `Left` and `Right` sets.
/// - `AccountId` is an account identifier type.
pub struct UnionOf<Left, Right, Criterion, AssetKind, AccountId>(
	core::marker::PhantomData<(Left, Right, Criterion, AssetKind, AccountId)>,
);

impl<
		Left: fungible::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Inspect<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type AssetId = AssetKind;
	type Balance = Left::Balance;

	fn total_issuance(asset: Self::AssetId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Inspect<AccountId>>::total_issuance(),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::total_issuance(a),
		}
	}
	fn active_issuance(asset: Self::AssetId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Inspect<AccountId>>::active_issuance(),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::active_issuance(a),
		}
	}
	fn minimum_balance(asset: Self::AssetId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Inspect<AccountId>>::minimum_balance(),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::minimum_balance(a),
		}
	}
	fn balance(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Inspect<AccountId>>::balance(who),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::balance(a, who),
		}
	}
	fn total_balance(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Inspect<AccountId>>::total_balance(who),
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
			Left(()) => {
				<Left as fungible::Inspect<AccountId>>::reducible_balance(who, preservation, force)
			},
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
			Left(()) => {
				<Left as fungible::Inspect<AccountId>>::can_deposit(who, amount, provenance)
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
			Left(()) => <Left as fungible::Inspect<AccountId>>::can_withdraw(who, amount),
			Right(a) => <Right as fungibles::Inspect<AccountId>>::can_withdraw(a, who, amount),
		}
	}
	fn asset_exists(asset: Self::AssetId) -> bool {
		match Criterion::convert(asset) {
			Left(()) => true,
			Right(a) => <Right as fungibles::Inspect<AccountId>>::asset_exists(a),
		}
	}
}

impl<
		Left: fungible::Inspect<AccountId> + fungible::metadata::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>
			+ fungibles::metadata::Inspect<AccountId>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::metadata::Inspect<AccountId>
	for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn name(asset: Self::AssetId) -> alloc::vec::Vec<u8> {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::metadata::Inspect<AccountId>>::name(),
			Right(a) => <Right as fungibles::metadata::Inspect<AccountId>>::name(a),
		}
	}
	fn symbol(asset: Self::AssetId) -> alloc::vec::Vec<u8> {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::metadata::Inspect<AccountId>>::symbol(),
			Right(a) => <Right as fungibles::metadata::Inspect<AccountId>>::symbol(a),
		}
	}
	fn decimals(asset: Self::AssetId) -> u8 {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::metadata::Inspect<AccountId>>::decimals(),
			Right(a) => <Right as fungibles::metadata::Inspect<AccountId>>::decimals(a),
		}
	}
}

impl<
		Left: fungible::Inspect<AccountId>
			+ fungible::metadata::Inspect<AccountId>
			+ fungible::metadata::Mutate<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance>
			+ fungibles::metadata::Inspect<AccountId>
			+ fungibles::metadata::Mutate<AccountId>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => {
				<Left as fungible::metadata::Mutate<AccountId>>::set(from, name, symbol, decimals)
			},
			Right(a) => <Right as fungibles::metadata::Mutate<AccountId>>::set(
				a, from, name, symbol, decimals,
			),
		}
	}
}

impl<
		Left: fungible::InspectHold<AccountId>,
		Right: fungibles::InspectHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => {
				<Left as fungible::InspectHold<AccountId>>::reducible_total_balance_on_hold(
					who, force,
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
			Left(()) => <Left as fungible::InspectHold<AccountId>>::hold_available(reason, who),
			Right(a) => {
				<Right as fungibles::InspectHold<AccountId>>::hold_available(a, reason, who)
			},
		}
	}
	fn total_balance_on_hold(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::InspectHold<AccountId>>::total_balance_on_hold(who),
			Right(a) => <Right as fungibles::InspectHold<AccountId>>::total_balance_on_hold(a, who),
		}
	}
	fn balance_on_hold(
		asset: Self::AssetId,
		reason: &Self::Reason,
		who: &AccountId,
	) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::InspectHold<AccountId>>::balance_on_hold(reason, who),
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
			Left(()) => <Left as fungible::InspectHold<AccountId>>::can_hold(reason, who, amount),
			Right(a) => {
				<Right as fungibles::InspectHold<AccountId>>::can_hold(a, reason, who, amount)
			},
		}
	}
}

impl<
		Left: fungible::InspectFreeze<AccountId>,
		Right: fungibles::InspectFreeze<AccountId, Balance = Left::Balance, Id = Left::Id>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::InspectFreeze<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type Id = Left::Id;
	fn balance_frozen(asset: Self::AssetId, id: &Self::Id, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::InspectFreeze<AccountId>>::balance_frozen(id, who),
			Right(a) => <Right as fungibles::InspectFreeze<AccountId>>::balance_frozen(a, id, who),
		}
	}
	fn balance_freezable(asset: Self::AssetId, who: &AccountId) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::InspectFreeze<AccountId>>::balance_freezable(who),
			Right(a) => <Right as fungibles::InspectFreeze<AccountId>>::balance_freezable(a, who),
		}
	}
	fn can_freeze(asset: Self::AssetId, id: &Self::Id, who: &AccountId) -> bool {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::InspectFreeze<AccountId>>::can_freeze(id, who),
			Right(a) => <Right as fungibles::InspectFreeze<AccountId>>::can_freeze(a, id, who),
		}
	}
}

impl<
		Left: fungible::Unbalanced<AccountId>,
		Right: fungibles::Unbalanced<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Unbalanced<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn handle_dust(dust: fungibles::Dust<AccountId, Self>)
	where
		Self: Sized,
	{
		match Criterion::convert(dust.0) {
			Left(()) => {
				<Left as fungible::Unbalanced<AccountId>>::handle_dust(fungible::Dust(dust.1))
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
			Left(()) => <Left as fungible::Unbalanced<AccountId>>::write_balance(who, amount),
			Right(a) => <Right as fungibles::Unbalanced<AccountId>>::write_balance(a, who, amount),
		}
	}
	fn set_total_issuance(asset: Self::AssetId, amount: Self::Balance) -> () {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Unbalanced<AccountId>>::set_total_issuance(amount),
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
			Left(()) => <Left as fungible::Unbalanced<AccountId>>::decrease_balance(
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
			Left(()) => {
				<Left as fungible::Unbalanced<AccountId>>::increase_balance(who, amount, precision)
			},
			Right(a) => <Right as fungibles::Unbalanced<AccountId>>::increase_balance(
				a, who, amount, precision,
			),
		}
	}
}

impl<
		Left: fungible::UnbalancedHold<AccountId>,
		Right: fungibles::UnbalancedHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => <Left as fungible::UnbalancedHold<AccountId>>::set_balance_on_hold(
				reason, who, amount,
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
			Left(()) => <Left as fungible::UnbalancedHold<AccountId>>::decrease_balance_on_hold(
				reason, who, amount, precision,
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
			Left(()) => <Left as fungible::UnbalancedHold<AccountId>>::increase_balance_on_hold(
				reason, who, amount, precision,
			),
			Right(a) => <Right as fungibles::UnbalancedHold<AccountId>>::increase_balance_on_hold(
				a, reason, who, amount, precision,
			),
		}
	}
}

impl<
		Left: fungible::Mutate<AccountId>,
		Right: fungibles::Mutate<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => <Left as fungible::Mutate<AccountId>>::mint_into(who, amount),
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
			Left(()) => <Left as fungible::Mutate<AccountId>>::burn_from(
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
			Left(()) => <Left as fungible::Mutate<AccountId>>::shelve(who, amount),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::shelve(a, who, amount),
		}
	}
	fn restore(
		asset: Self::AssetId,
		who: &AccountId,
		amount: Self::Balance,
	) -> Result<Self::Balance, DispatchError> {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::Mutate<AccountId>>::restore(who, amount),
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
			Left(()) => {
				<Left as fungible::Mutate<AccountId>>::transfer(source, dest, amount, preservation)
			},
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
			Left(()) => <Left as fungible::Mutate<AccountId>>::set_balance(who, amount),
			Right(a) => <Right as fungibles::Mutate<AccountId>>::set_balance(a, who, amount),
		}
	}
}

impl<
		Left: fungible::MutateHold<AccountId>,
		Right: fungibles::MutateHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => <Left as fungible::MutateHold<AccountId>>::hold(reason, who, amount),
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
			Left(()) => {
				<Left as fungible::MutateHold<AccountId>>::release(reason, who, amount, precision)
			},
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
			Left(()) => <Left as fungible::MutateHold<AccountId>>::burn_held(
				reason, who, amount, precision, force,
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
			Left(()) => <Left as fungible::MutateHold<AccountId>>::transfer_on_hold(
				reason, source, dest, amount, precision, mode, force,
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
			Left(()) => <Left as fungible::MutateHold<AccountId>>::transfer_and_hold(
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
		Left: fungible::MutateFreeze<AccountId>,
		Right: fungibles::MutateFreeze<AccountId, Balance = Left::Balance, Id = Left::Id>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => <Left as fungible::MutateFreeze<AccountId>>::set_freeze(id, who, amount),
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
			Left(()) => <Left as fungible::MutateFreeze<AccountId>>::extend_freeze(id, who, amount),
			Right(a) => {
				<Right as fungibles::MutateFreeze<AccountId>>::extend_freeze(a, id, who, amount)
			},
		}
	}
	fn thaw(asset: Self::AssetId, id: &Self::Id, who: &AccountId) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(()) => <Left as fungible::MutateFreeze<AccountId>>::thaw(id, who),
			Right(a) => <Right as fungibles::MutateFreeze<AccountId>>::thaw(a, id, who),
		}
	}
}

pub struct ConvertImbalanceDropHandler<
	Left,
	Right,
	Criterion,
	AssetKind,
	Balance,
	AssetId,
	AccountId,
>(core::marker::PhantomData<(Left, Right, Criterion, AssetKind, Balance, AssetId, AccountId)>);

impl<
		Left: fungible::HandleImbalanceDrop<Balance>,
		Right: fungibles::HandleImbalanceDrop<AssetId, Balance>,
		Criterion: Convert<AssetKind, Either<(), AssetId>>,
		AssetKind,
		Balance,
		AssetId,
		AccountId,
	> fungibles::HandleImbalanceDrop<AssetKind, Balance>
	for ConvertImbalanceDropHandler<Left, Right, Criterion, AssetKind, Balance, AssetId, AccountId>
{
	fn handle(asset: AssetKind, amount: Balance) {
		match Criterion::convert(asset) {
			Left(()) => Left::handle(amount),
			Right(a) => Right::handle(a, amount),
		}
	}
}

impl<
		Left: fungible::Balanced<AccountId>,
		Right: fungibles::Balanced<AccountId, Balance = Left::Balance>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Balanced<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type OnDropDebt = ConvertImbalanceDropHandler<
		Left::OnDropDebt,
		Right::OnDropDebt,
		Criterion,
		AssetKind,
		Left::Balance,
		Right::AssetId,
		AccountId,
	>;
	type OnDropCredit = ConvertImbalanceDropHandler<
		Left::OnDropCredit,
		Right::OnDropCredit,
		Criterion,
		AssetKind,
		Left::Balance,
		Right::AssetId,
		AccountId,
	>;

	fn deposit(
		asset: Self::AssetId,
		who: &AccountId,
		value: Self::Balance,
		precision: Precision,
	) -> Result<fungibles::Debt<AccountId, Self>, DispatchError> {
		match Criterion::convert(asset.clone()) {
			Left(()) => <Left as fungible::Balanced<AccountId>>::deposit(who, value, precision)
				.map(|d| fungibles::imbalance::from_fungible(d, asset)),
			Right(a) => {
				<Right as fungibles::Balanced<AccountId>>::deposit(a, who, value, precision)
					.map(|d| fungibles::imbalance::from_fungibles(d, asset))
			},
		}
	}
	fn issue(asset: Self::AssetId, amount: Self::Balance) -> fungibles::Credit<AccountId, Self> {
		match Criterion::convert(asset.clone()) {
			Left(()) => {
				let credit = <Left as fungible::Balanced<AccountId>>::issue(amount);
				fungibles::imbalance::from_fungible(credit, asset)
			},
			Right(a) => {
				let credit = <Right as fungibles::Balanced<AccountId>>::issue(a, amount);
				fungibles::imbalance::from_fungibles(credit, asset)
			},
		}
	}
	fn pair(
		asset: Self::AssetId,
		amount: Self::Balance,
	) -> Result<(fungibles::Debt<AccountId, Self>, fungibles::Credit<AccountId, Self>), DispatchError>
	{
		match Criterion::convert(asset.clone()) {
			Left(()) => {
				let (a, b) = <Left as fungible::Balanced<AccountId>>::pair(amount)?;
				Ok((
					fungibles::imbalance::from_fungible(a, asset.clone()),
					fungibles::imbalance::from_fungible(b, asset),
				))
			},
			Right(a) => {
				let (a, b) = <Right as fungibles::Balanced<AccountId>>::pair(a, amount)?;
				Ok((
					fungibles::imbalance::from_fungibles(a, asset.clone()),
					fungibles::imbalance::from_fungibles(b, asset),
				))
			},
		}
	}
	fn rescind(asset: Self::AssetId, amount: Self::Balance) -> fungibles::Debt<AccountId, Self> {
		match Criterion::convert(asset.clone()) {
			Left(()) => {
				let debt = <Left as fungible::Balanced<AccountId>>::rescind(amount);
				fungibles::imbalance::from_fungible(debt, asset)
			},
			Right(a) => {
				let debt = <Right as fungibles::Balanced<AccountId>>::rescind(a, amount);
				fungibles::imbalance::from_fungibles(debt, asset)
			},
		}
	}
	fn resolve(
		who: &AccountId,
		credit: fungibles::Credit<AccountId, Self>,
	) -> Result<(), fungibles::Credit<AccountId, Self>> {
		let asset = credit.asset();
		match Criterion::convert(asset.clone()) {
			Left(()) => {
				let credit = imbalance::from_fungibles(credit);
				<Left as fungible::Balanced<AccountId>>::resolve(who, credit)
					.map_err(|credit| fungibles::imbalance::from_fungible(credit, asset))
			},
			Right(a) => {
				let credit = fungibles::imbalance::from_fungibles(credit, a);
				<Right as fungibles::Balanced<AccountId>>::resolve(who, credit)
					.map_err(|credit| fungibles::imbalance::from_fungibles(credit, asset))
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
			Left(()) => {
				let debt = imbalance::from_fungibles(debt);
				match <Left as fungible::Balanced<AccountId>>::settle(who, debt, preservation) {
					Ok(c) => Ok(fungibles::imbalance::from_fungible(c, asset)),
					Err(d) => Err(fungibles::imbalance::from_fungible(d, asset)),
				}
			},
			Right(a) => {
				let debt = fungibles::imbalance::from_fungibles(debt, a);
				match <Right as fungibles::Balanced<AccountId>>::settle(who, debt, preservation) {
					Ok(c) => Ok(fungibles::imbalance::from_fungibles(c, asset)),
					Err(d) => Err(fungibles::imbalance::from_fungibles(d, asset)),
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
			Left(()) => <Left as fungible::Balanced<AccountId>>::withdraw(
				who,
				value,
				precision,
				preservation,
				force,
			)
			.map(|c| fungibles::imbalance::from_fungible(c, asset)),
			Right(a) => <Right as fungibles::Balanced<AccountId>>::withdraw(
				a,
				who,
				value,
				precision,
				preservation,
				force,
			)
			.map(|c| fungibles::imbalance::from_fungibles(c, asset)),
		}
	}
}

impl<
		Left: fungible::BalancedHold<AccountId>
			+ fungible::hold::DoneSlash<Self::Reason, AccountId, Self::Balance>,
		Right: fungibles::BalancedHold<AccountId, Balance = Left::Balance, Reason = Left::Reason>
			+ fungibles::hold::DoneSlash<AssetKind, Left::Reason, AccountId, Left::Balance>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			Left(()) => {
				let (credit, amount) =
					<Left as fungible::BalancedHold<AccountId>>::slash(reason, who, amount);
				(fungibles::imbalance::from_fungible(credit, asset), amount)
			},
			Right(a) => {
				let (credit, amount) =
					<Right as fungibles::BalancedHold<AccountId>>::slash(a, reason, who, amount);
				(fungibles::imbalance::from_fungibles(credit, asset), amount)
			},
		}
	}
}
impl<
		Reason,
		Balance,
		Left: fungible::hold::DoneSlash<Reason, AccountId, Balance>,
		Right: fungibles::hold::DoneSlash<Right::AssetId, Reason, AccountId, Balance>
			+ fungibles::Inspect<AccountId>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::hold::DoneSlash<AssetKind, Reason, AccountId, Balance>
	for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	fn done_slash(asset: AssetKind, reason: &Reason, who: &AccountId, amount: Balance) {
		match Criterion::convert(asset.clone()) {
			Left(()) => {
				Left::done_slash(reason, who, amount);
			},
			Right(a) => {
				Right::done_slash(a, reason, who, amount);
			},
		}
	}
}

impl<
		Left: fungible::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId, Balance = Left::Balance> + fungibles::Create<AccountId>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
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
			// no-op for `Left` since `Create` trait is not defined within `fungible::*`.
			Left(()) => Ok(()),
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
		Left: fungible::Inspect<AccountId>
			+ AccountTouch<(), AccountId, Balance = <Left as fungible::Inspect<AccountId>>::Balance>,
		Right: fungibles::Inspect<AccountId>
			+ AccountTouch<
				Right::AssetId,
				AccountId,
				Balance = <Left as fungible::Inspect<AccountId>>::Balance,
			>,
		Criterion: Convert<AssetKind, Either<(), Right::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> AccountTouch<AssetKind, AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type Balance = <Left as fungible::Inspect<AccountId>>::Balance;

	fn deposit_required(asset: AssetKind) -> Self::Balance {
		match Criterion::convert(asset) {
			Left(()) => <Left as AccountTouch<(), AccountId>>::deposit_required(()),
			Right(a) => <Right as AccountTouch<Right::AssetId, AccountId>>::deposit_required(a),
		}
	}

	fn should_touch(asset: AssetKind, who: &AccountId) -> bool {
		match Criterion::convert(asset) {
			Left(()) => <Left as AccountTouch<(), AccountId>>::should_touch((), who),
			Right(a) => <Right as AccountTouch<Right::AssetId, AccountId>>::should_touch(a, who),
		}
	}

	fn touch(asset: AssetKind, who: &AccountId, depositor: &AccountId) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(()) => <Left as AccountTouch<(), AccountId>>::touch((), who, depositor),
			Right(a) => {
				<Right as AccountTouch<Right::AssetId, AccountId>>::touch(a, who, depositor)
			},
		}
	}
}

impl<
		Left: fungible::Inspect<AccountId>,
		Right: fungibles::Inspect<AccountId> + fungibles::Refund<AccountId>,
		Criterion: Convert<AssetKind, Either<(), <Right as fungibles::Refund<AccountId>>::AssetId>>,
		AssetKind: AssetId,
		AccountId,
	> fungibles::Refund<AccountId> for UnionOf<Left, Right, Criterion, AssetKind, AccountId>
{
	type AssetId = AssetKind;
	type Balance = <Right as fungibles::Refund<AccountId>>::Balance;

	fn deposit_held(asset: AssetKind, who: AccountId) -> Option<(AccountId, Self::Balance)> {
		match Criterion::convert(asset) {
			Left(()) => None,
			Right(a) => <Right as fungibles::Refund<AccountId>>::deposit_held(a, who),
		}
	}
	fn refund(asset: AssetKind, who: AccountId) -> DispatchResult {
		match Criterion::convert(asset) {
			Left(()) => Err(DispatchError::Unavailable),
			Right(a) => <Right as fungibles::Refund<AccountId>>::refund(a, who),
		}
	}
}
