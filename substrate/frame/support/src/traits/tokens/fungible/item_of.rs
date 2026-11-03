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

//! Adapter to use `fungibles::*` implementations as `fungible::*`.
//!
//! This allows for a `fungibles` asset, e.g. from the `pallet_assets` pallet, to be used when a
//! `fungible` asset is expected.
//!
//! See the [`crate::traits::fungible`] doc for more information about fungible traits.

use super::*;
use crate::traits::{
	fungible::imbalance,
	tokens::{
		fungibles, DepositConsequence, Fortitude, Precision, Preservation, Provenance, Restriction,
		WithdrawConsequence,
	},
};
use alloc::vec::Vec;
use frame_support::traits::fungible::hold::DoneSlash;
use sp_core::Get;
use sp_runtime::{DispatchError, DispatchResult};

/// Convert a `fungibles` trait implementation into a `fungible` trait implementation by identifying
/// a single item.
pub struct ItemOf<
	F: fungibles::Inspect<AccountId>,
	A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
	AccountId,
>(core::marker::PhantomData<(F, A, AccountId)>);

impl<
		F: fungibles::Inspect<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> Inspect<AccountId> for ItemOf<F, A, AccountId>
{
	type Balance = <F as fungibles::Inspect<AccountId>>::Balance;
	fn total_issuance() -> Self::Balance {
		<F as fungibles::Inspect<AccountId>>::total_issuance(A::get())
	}
	fn active_issuance() -> Self::Balance {
		<F as fungibles::Inspect<AccountId>>::active_issuance(A::get())
	}
	fn minimum_balance() -> Self::Balance {
		<F as fungibles::Inspect<AccountId>>::minimum_balance(A::get())
	}
	fn balance(who: &AccountId) -> Self::Balance {
		<F as fungibles::Inspect<AccountId>>::balance(A::get(), who)
	}
	fn total_balance(who: &AccountId) -> Self::Balance {
		<F as fungibles::Inspect<AccountId>>::total_balance(A::get(), who)
	}
	fn reducible_balance(
		who: &AccountId,
		preservation: Preservation,
		force: Fortitude,
	) -> Self::Balance {
		<F as fungibles::Inspect<AccountId>>::reducible_balance(A::get(), who, preservation, force)
	}
	fn can_deposit(
		who: &AccountId,
		amount: Self::Balance,
		provenance: Provenance,
	) -> DepositConsequence {
		<F as fungibles::Inspect<AccountId>>::can_deposit(A::get(), who, amount, provenance)
	}
	fn can_withdraw(who: &AccountId, amount: Self::Balance) -> WithdrawConsequence<Self::Balance> {
		<F as fungibles::Inspect<AccountId>>::can_withdraw(A::get(), who, amount)
	}
}

impl<
		F: fungibles::InspectHold<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> InspectHold<AccountId> for ItemOf<F, A, AccountId>
{
	type Reason = F::Reason;

	fn reducible_total_balance_on_hold(who: &AccountId, force: Fortitude) -> Self::Balance {
		<F as fungibles::InspectHold<AccountId>>::reducible_total_balance_on_hold(
			A::get(),
			who,
			force,
		)
	}
	fn hold_available(reason: &Self::Reason, who: &AccountId) -> bool {
		<F as fungibles::InspectHold<AccountId>>::hold_available(A::get(), reason, who)
	}
	fn total_balance_on_hold(who: &AccountId) -> Self::Balance {
		<F as fungibles::InspectHold<AccountId>>::total_balance_on_hold(A::get(), who)
	}
	fn balance_on_hold(reason: &Self::Reason, who: &AccountId) -> Self::Balance {
		<F as fungibles::InspectHold<AccountId>>::balance_on_hold(A::get(), reason, who)
	}
	fn can_hold(reason: &Self::Reason, who: &AccountId, amount: Self::Balance) -> bool {
		<F as fungibles::InspectHold<AccountId>>::can_hold(A::get(), reason, who, amount)
	}
}

impl<
		F: fungibles::InspectFreeze<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> InspectFreeze<AccountId> for ItemOf<F, A, AccountId>
{
	type Id = F::Id;
	fn balance_frozen(id: &Self::Id, who: &AccountId) -> Self::Balance {
		<F as fungibles::InspectFreeze<AccountId>>::balance_frozen(A::get(), id, who)
	}
	fn balance_freezable(who: &AccountId) -> Self::Balance {
		<F as fungibles::InspectFreeze<AccountId>>::balance_freezable(A::get(), who)
	}
	fn can_freeze(id: &Self::Id, who: &AccountId) -> bool {
		<F as fungibles::InspectFreeze<AccountId>>::can_freeze(A::get(), id, who)
	}
}

impl<
		F: fungibles::Unbalanced<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> Unbalanced<AccountId> for ItemOf<F, A, AccountId>
{
	fn handle_dust(dust: regular::Dust<AccountId, Self>)
	where
		Self: Sized,
	{
		<F as fungibles::Unbalanced<AccountId>>::handle_dust(fungibles::Dust(A::get(), dust.0))
	}
	fn write_balance(
		who: &AccountId,
		amount: Self::Balance,
	) -> Result<Option<Self::Balance>, DispatchError> {
		<F as fungibles::Unbalanced<AccountId>>::write_balance(A::get(), who, amount)
	}
	fn set_total_issuance(amount: Self::Balance) -> () {
		<F as fungibles::Unbalanced<AccountId>>::set_total_issuance(A::get(), amount)
	}
	fn decrease_balance(
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		preservation: Preservation,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Unbalanced<AccountId>>::decrease_balance(
			A::get(),
			who,
			amount,
			precision,
			preservation,
			force,
		)
	}
	fn increase_balance(
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Unbalanced<AccountId>>::increase_balance(A::get(), who, amount, precision)
	}
}

impl<
		F: fungibles::UnbalancedHold<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> UnbalancedHold<AccountId> for ItemOf<F, A, AccountId>
{
	fn set_balance_on_hold(
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
	) -> DispatchResult {
		<F as fungibles::UnbalancedHold<AccountId>>::set_balance_on_hold(
			A::get(),
			reason,
			who,
			amount,
		)
	}
	fn decrease_balance_on_hold(
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::UnbalancedHold<AccountId>>::decrease_balance_on_hold(
			A::get(),
			reason,
			who,
			amount,
			precision,
		)
	}
	fn increase_balance_on_hold(
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::UnbalancedHold<AccountId>>::increase_balance_on_hold(
			A::get(),
			reason,
			who,
			amount,
			precision,
		)
	}
}

impl<
		F: fungibles::Mutate<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId: Eq,
	> Mutate<AccountId> for ItemOf<F, A, AccountId>
{
	fn mint_into(who: &AccountId, amount: Self::Balance) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Mutate<AccountId>>::mint_into(A::get(), who, amount)
	}
	fn burn_from(
		who: &AccountId,
		amount: Self::Balance,
		preservation: Preservation,
		precision: Precision,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Mutate<AccountId>>::burn_from(
			A::get(),
			who,
			amount,
			preservation,
			precision,
			force,
		)
	}
	fn shelve(who: &AccountId, amount: Self::Balance) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Mutate<AccountId>>::shelve(A::get(), who, amount)
	}
	fn restore(who: &AccountId, amount: Self::Balance) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Mutate<AccountId>>::restore(A::get(), who, amount)
	}
	fn transfer(
		source: &AccountId,
		dest: &AccountId,
		amount: Self::Balance,
		preservation: Preservation,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::Mutate<AccountId>>::transfer(A::get(), source, dest, amount, preservation)
	}

	fn set_balance(who: &AccountId, amount: Self::Balance) -> Self::Balance {
		<F as fungibles::Mutate<AccountId>>::set_balance(A::get(), who, amount)
	}
}

impl<
		F: fungibles::MutateHold<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> MutateHold<AccountId> for ItemOf<F, A, AccountId>
{
	fn hold(reason: &Self::Reason, who: &AccountId, amount: Self::Balance) -> DispatchResult {
		<F as fungibles::MutateHold<AccountId>>::hold(A::get(), reason, who, amount)
	}
	fn release(
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::MutateHold<AccountId>>::release(A::get(), reason, who, amount, precision)
	}
	fn burn_held(
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::MutateHold<AccountId>>::burn_held(
			A::get(),
			reason,
			who,
			amount,
			precision,
			force,
		)
	}
	fn transfer_on_hold(
		reason: &Self::Reason,
		source: &AccountId,
		dest: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		mode: Restriction,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::MutateHold<AccountId>>::transfer_on_hold(
			A::get(),
			reason,
			source,
			dest,
			amount,
			precision,
			mode,
			force,
		)
	}
	fn transfer_and_hold(
		reason: &Self::Reason,
		source: &AccountId,
		dest: &AccountId,
		amount: Self::Balance,
		precision: Precision,
		preservation: Preservation,
		force: Fortitude,
	) -> Result<Self::Balance, DispatchError> {
		<F as fungibles::MutateHold<AccountId>>::transfer_and_hold(
			A::get(),
			reason,
			source,
			dest,
			amount,
			precision,
			preservation,
			force,
		)
	}
}

impl<
		F: fungibles::MutateFreeze<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> MutateFreeze<AccountId> for ItemOf<F, A, AccountId>
{
	fn set_freeze(id: &Self::Id, who: &AccountId, amount: Self::Balance) -> DispatchResult {
		<F as fungibles::MutateFreeze<AccountId>>::set_freeze(A::get(), id, who, amount)
	}
	fn extend_freeze(id: &Self::Id, who: &AccountId, amount: Self::Balance) -> DispatchResult {
		<F as fungibles::MutateFreeze<AccountId>>::extend_freeze(A::get(), id, who, amount)
	}
	fn thaw(id: &Self::Id, who: &AccountId) -> DispatchResult {
		<F as fungibles::MutateFreeze<AccountId>>::thaw(A::get(), id, who)
	}
}

pub struct ConvertImbalanceDropHandler<AccountId, Balance, AssetIdType, AssetId, Handler>(
	core::marker::PhantomData<(AccountId, Balance, AssetIdType, AssetId, Handler)>,
);

impl<
		AccountId,
		Balance,
		AssetIdType,
		AssetId: Get<AssetIdType>,
		Handler: crate::traits::fungibles::HandleImbalanceDrop<AssetIdType, Balance>,
	> HandleImbalanceDrop<Balance>
	for ConvertImbalanceDropHandler<AccountId, Balance, AssetIdType, AssetId, Handler>
{
	fn handle(amount: Balance) {
		Handler::handle(AssetId::get(), amount)
	}
}

impl<
		F: fungibles::Inspect<AccountId>
			+ fungibles::Unbalanced<AccountId>
			+ fungibles::Balanced<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> Balanced<AccountId> for ItemOf<F, A, AccountId>
{
	type OnDropDebt =
		ConvertImbalanceDropHandler<AccountId, Self::Balance, F::AssetId, A, F::OnDropDebt>;
	type OnDropCredit =
		ConvertImbalanceDropHandler<AccountId, Self::Balance, F::AssetId, A, F::OnDropCredit>;
	fn deposit(
		who: &AccountId,
		value: Self::Balance,
		precision: Precision,
	) -> Result<Debt<AccountId, Self>, DispatchError> {
		<F as fungibles::Balanced<AccountId>>::deposit(A::get(), who, value, precision)
			.map(imbalance::from_fungibles)
	}
	fn issue(amount: Self::Balance) -> Credit<AccountId, Self> {
		let credit = <F as fungibles::Balanced<AccountId>>::issue(A::get(), amount);
		imbalance::from_fungibles(credit)
	}
	fn pair(
		amount: Self::Balance,
	) -> Result<(Debt<AccountId, Self>, Credit<AccountId, Self>), DispatchError> {
		let (a, b) = <F as fungibles::Balanced<AccountId>>::pair(A::get(), amount)?;
		Ok((imbalance::from_fungibles(a), imbalance::from_fungibles(b)))
	}
	fn rescind(amount: Self::Balance) -> Debt<AccountId, Self> {
		let debt = <F as fungibles::Balanced<AccountId>>::rescind(A::get(), amount);
		imbalance::from_fungibles(debt)
	}
	fn resolve(
		who: &AccountId,
		credit: Credit<AccountId, Self>,
	) -> Result<(), Credit<AccountId, Self>> {
		let credit = fungibles::imbalance::from_fungible(credit, A::get());
		<F as fungibles::Balanced<AccountId>>::resolve(who, credit)
			.map_err(imbalance::from_fungibles)
	}
	fn settle(
		who: &AccountId,
		debt: Debt<AccountId, Self>,
		preservation: Preservation,
	) -> Result<Credit<AccountId, Self>, Debt<AccountId, Self>> {
		let debt = fungibles::imbalance::from_fungible(debt, A::get());
		<F as fungibles::Balanced<AccountId>>::settle(who, debt, preservation).map_or_else(
			|d| Err(imbalance::from_fungibles(d)),
			|c| Ok(imbalance::from_fungibles(c)),
		)
	}
	fn withdraw(
		who: &AccountId,
		value: Self::Balance,
		precision: Precision,
		preservation: Preservation,
		force: Fortitude,
	) -> Result<Credit<AccountId, Self>, DispatchError> {
		<F as fungibles::Balanced<AccountId>>::withdraw(
			A::get(),
			who,
			value,
			precision,
			preservation,
			force,
		)
		.map(imbalance::from_fungibles)
	}
}

impl<
		F: fungibles::BalancedHold<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> BalancedHold<AccountId> for ItemOf<F, A, AccountId>
{
	fn slash(
		reason: &Self::Reason,
		who: &AccountId,
		amount: Self::Balance,
	) -> (Credit<AccountId, Self>, Self::Balance) {
		let (credit, amount) =
			<F as fungibles::BalancedHold<AccountId>>::slash(A::get(), reason, who, amount);
		(imbalance::from_fungibles(credit), amount)
	}
}

impl<
		F: fungibles::BalancedHold<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> DoneSlash<F::Reason, AccountId, F::Balance> for ItemOf<F, A, AccountId>
{
	fn done_slash(reason: &F::Reason, who: &AccountId, amount: F::Balance) {
		<F as fungibles::hold::DoneSlash<F::AssetId, F::Reason, AccountId, F::Balance>>::done_slash(
			A::get(),
			reason,
			who,
			amount,
		)
	}
}

/// Adapter implementation of [`super::metadata::Inspect`] for [`ItemOf`].
///
/// See the original trait documentation for information on item meaning and usage.
impl<
		F: fungibles::metadata::Inspect<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> super::metadata::Inspect<AccountId> for ItemOf<F, A, AccountId>
{
	/// See [`fungibles::metadata::Inspect::name`].
	fn name() -> Vec<u8> {
		<F as fungibles::metadata::Inspect<AccountId>>::name(A::get())
	}
	/// See [`fungibles::metadata::Inspect::symbol`].
	fn symbol() -> Vec<u8> {
		<F as fungibles::metadata::Inspect<AccountId>>::symbol(A::get())
	}
	/// See [`fungibles::metadata::Inspect::decimals`].
	fn decimals() -> u8 {
		<F as fungibles::metadata::Inspect<AccountId>>::decimals(A::get())
	}
}

/// Adapter implementation of [`super::metadata::Mutate`] for [`ItemOf`].
///
/// See the original trait documentation for information on item meaning and usage.
impl<
		F: fungibles::metadata::Mutate<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> super::metadata::Mutate<AccountId> for ItemOf<F, A, AccountId>
{
	/// See [`fungibles::metadata::Mutate::set`].
	fn set(from: &AccountId, name: Vec<u8>, symbol: Vec<u8>, decimals: u8) -> DispatchResult {
		<F as fungibles::metadata::Mutate<AccountId>>::set(A::get(), from, name, symbol, decimals)
	}
}

/// Adapter implementation of [`super::lifetime::Create`] for [`ItemOf`].
///
/// See the original trait documentation for information on item meaning and usage.
impl<
		F: fungibles::Create<AccountId>,
		A: Get<<F as fungibles::Inspect<AccountId>>::AssetId>,
		AccountId,
	> super::lifetime::Create<AccountId> for ItemOf<F, A, AccountId>
{
	/// See [`fungibles::Create::create`].
	fn create(admin: AccountId, is_sufficient: bool, min_balance: Self::Balance) -> DispatchResult {
		<F as fungibles::Create<AccountId>>::create(A::get(), admin, is_sufficient, min_balance)
	}
}

#[test]
fn test() {}
