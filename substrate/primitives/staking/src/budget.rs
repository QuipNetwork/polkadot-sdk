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

//! Traits for inflation issuance and budget distribution.
//!
//! These traits define how new tokens are minted and distributed among budget recipients
//! (e.g. staker rewards, validator incentives).

use alloc::vec::Vec;
use sp_runtime::BoundedVec;

/// Maximum length of a budget key identifier.
pub const MAX_BUDGET_KEY_LEN: u32 = 32;

/// Identifier for a budget category in the inflation distribution system.
///
/// Each budget recipient (e.g. staker rewards, validator incentive) is identified
/// by a unique key. Keys are bounded to [`MAX_BUDGET_KEY_LEN`] bytes.
pub type BudgetKey = BoundedVec<u8, sp_core::ConstU32<MAX_BUDGET_KEY_LEN>>;

/// Computes new token issuance for a given time period.
///
/// Unlike [`super::EraPayout`], this trait does not depend on staking state. Issuance is
/// purely a function of total supply and elapsed time.
pub trait IssuanceCurve<Balance> {
	/// Compute how much new tokens to mint for the given period.
	fn issue(total_issuance: Balance, elapsed_millis: u64) -> Balance;
}

impl<Balance: Default> IssuanceCurve<Balance> for () {
	fn issue(_total_issuance: Balance, _elapsed_millis: u64) -> Balance {
		Default::default()
	}
}

/// A recipient of inflation budget.
///
/// Pallets that want a share of inflation implement this trait, providing a unique key
/// and a pot account where minted funds are deposited.
pub trait BudgetRecipient<AccountId> {
	/// Unique identifier for this budget category.
	fn budget_key() -> BudgetKey;
	/// The account that receives minted inflation funds.
	fn pot_account() -> AccountId;
}

/// Aggregates multiple [`BudgetRecipient`]s into a list.
///
/// Implemented for tuples of `BudgetRecipient` types, allowing runtime configuration like:
/// ```ignore
/// type BudgetRecipients = (StakerRewardRecipient, ValidatorIncentiveRecipient);
/// ```
pub trait BudgetRecipientList<AccountId> {
	/// Collect all registered recipients as `(key, account)` pairs.
	fn recipients() -> Vec<(BudgetKey, AccountId)>;
}

impl<AccountId> BudgetRecipientList<AccountId> for () {
	fn recipients() -> Vec<(BudgetKey, AccountId)> {
		Vec::new()
	}
}

#[impl_trait_for_tuples::impl_for_tuples(1, 10)]
#[tuple_types_custom_trait_bound(BudgetRecipient<AccountId>)]
impl<AccountId> BudgetRecipientList<AccountId> for Tuple {
	fn recipients() -> Vec<(BudgetKey, AccountId)> {
		let mut v = Vec::new();
		for_tuples!( #( v.push((Tuple::budget_key(), Tuple::pot_account())); )* );
		debug_assert!(
			{
				let mut keys: Vec<_> = v.iter().map(|(k, _)| k.clone()).collect();
				keys.sort();
				keys.windows(2).all(|w| w[0] != w[1])
			},
			"Duplicate BudgetRecipient key detected"
		);
		v
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct RecipientA;
	impl BudgetRecipient<u64> for RecipientA {
		fn budget_key() -> BudgetKey {
			BudgetKey::truncate_from(b"alpha".to_vec())
		}
		fn pot_account() -> u64 {
			1
		}
	}

	struct RecipientB;
	impl BudgetRecipient<u64> for RecipientB {
		fn budget_key() -> BudgetKey {
			BudgetKey::truncate_from(b"beta".to_vec())
		}
		fn pot_account() -> u64 {
			2
		}
	}

	// Duplicate key: same as RecipientA.
	struct RecipientDuplicate;
	impl BudgetRecipient<u64> for RecipientDuplicate {
		fn budget_key() -> BudgetKey {
			BudgetKey::truncate_from(b"alpha".to_vec())
		}
		fn pot_account() -> u64 {
			3
		}
	}

	#[test]
	fn unique_keys_work() {
		let recipients = <(RecipientA, RecipientB) as BudgetRecipientList<u64>>::recipients();
		assert_eq!(recipients.len(), 2);
	}

	#[test]
	#[should_panic(expected = "Duplicate BudgetRecipient key detected")]
	fn duplicate_keys_panics() {
		let _ = <(RecipientA, RecipientDuplicate) as BudgetRecipientList<u64>>::recipients();
	}
}
