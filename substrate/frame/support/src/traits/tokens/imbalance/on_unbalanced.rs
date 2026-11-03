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

//! Trait for handling imbalances.

use core::marker::PhantomData;
use frame_support::traits::{fungible, fungibles, misc::TryDrop};
use sp_core::TypedGet;

/// Handler for when some currency "account" decreased in balance for
/// some reason.
///
/// The only reason at present for an increase would be for validator rewards, but
/// there may be other reasons in the future or for other chains.
///
/// Reasons for decreases include:
///
/// - Someone got slashed.
/// - Someone paid for a transaction to be included.
pub trait OnUnbalanced<Imbalance: TryDrop> {
	/// Handler for some imbalances. The different imbalances might have different origins or
	/// meanings, dependent on the context. Will default to simply calling on_unbalanced for all
	/// of them. Infallible.
	fn on_unbalanceds(mut amounts: impl Iterator<Item = Imbalance>)
	where
		Imbalance: crate::traits::tokens::imbalance::TryMerge,
	{
		let mut sum: Option<Imbalance> = None;
		while let Some(next) = amounts.next() {
			sum = match sum {
				Some(sum) => match sum.try_merge(next) {
					Ok(sum) => Some(sum),
					Err((sum, next)) => {
						Self::on_unbalanced(next);
						Some(sum)
					},
				},
				None => Some(next),
			}
		}
		if let Some(sum) = sum {
			Self::on_unbalanced(sum)
		}
	}

	/// Handler for some imbalance. Infallible.
	fn on_unbalanced(amount: Imbalance) {
		amount.try_drop().unwrap_or_else(Self::on_nonzero_unbalanced)
	}

	/// Actually handle a non-zero imbalance. You probably want to implement this rather than
	/// `on_unbalanced`.
	fn on_nonzero_unbalanced(amount: Imbalance) {
		drop(amount);
	}
}

impl<Imbalance: TryDrop> OnUnbalanced<Imbalance> for () {}

/// Resolves received asset credit to account `A`, implementing [`OnUnbalanced`].
///
/// Credits that cannot be resolved to account `A` are dropped. This may occur if the account for
/// address `A` does not exist and the existential deposit requirement is not met.
pub struct ResolveTo<A, F>(PhantomData<(A, F)>);
impl<A: TypedGet, F: fungible::Balanced<A::Type>> OnUnbalanced<fungible::Credit<A::Type, F>>
	for ResolveTo<A, F>
{
	fn on_nonzero_unbalanced(credit: fungible::Credit<A::Type, F>) {
		let _ = F::resolve(&A::get(), credit).map_err(|c| drop(c));
	}
}

/// Resolves received asset credit to account `A`, implementing [`OnUnbalanced`].
///
/// Credits that cannot be resolved to account `A` are dropped. This may occur if the account for
/// address `A` does not exist and the existential deposit requirement is not met.
pub struct ResolveAssetTo<A, F>(PhantomData<(A, F)>);
impl<A: TypedGet, F: fungibles::Balanced<A::Type>> OnUnbalanced<fungibles::Credit<A::Type, F>>
	for ResolveAssetTo<A, F>
{
	fn on_nonzero_unbalanced(credit: fungibles::Credit<A::Type, F>) {
		let _ = F::resolve(&A::get(), credit).map_err(|c| drop(c));
	}
}

/// Resolves received asset credit to account `Target` if `Target::get()` returns some, implementing
/// [`OnUnbalanced`].
///
/// Credits that cannot be resolved to account `A` are dropped. This may occur if the account for
/// address `A` does not exist and the existential deposit requirement is not met.
pub struct MaybeResolveAssetTo<Target, F, AccountId>(PhantomData<(Target, F, AccountId)>);
impl<Target: TypedGet<Type = Option<AccountId>>, F: fungibles::Balanced<AccountId>, AccountId>
	OnUnbalanced<fungibles::Credit<AccountId, F>> for MaybeResolveAssetTo<Target, F, AccountId>
{
	fn on_nonzero_unbalanced(credit: fungibles::Credit<AccountId, F>) {
		if let Some(account) = Target::get() {
			let _ = F::resolve(&account, credit).map_err(|c| drop(c));
		} else {
			drop(credit);
		}
	}
}
