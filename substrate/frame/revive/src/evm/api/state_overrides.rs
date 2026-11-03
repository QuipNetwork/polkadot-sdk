// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::Bytes;
use alloc::collections::BTreeMap;
use ethereum_types::*;
use pallet_revive_types::runtime_api::*;

/// A mapping from account addresses to their state overrides, used to temporarily modify account
/// state during `eth_call` and similar simulation methods without affecting on-chain data.
///
/// Each entry maps an [`Address`] to a [`StateOverride`] that specifies which parts of the
/// account's state to replace for the duration of the call.
///
/// Conforms to the [Geth state override set specification](https://geth.ethereum.org/docs/interacting-with-geth/rpc/objects#state-override-set).
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct StateOverrideSet(pub BTreeMap<Address, StateOverride>);

impl core::ops::Deref for StateOverrideSet {
	type Target = BTreeMap<Address, StateOverride>;

	fn deref(&self) -> &Self::Target {
		&self.0
	}
}

impl core::ops::DerefMut for StateOverrideSet {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.0
	}
}

impl From<StateOverrideSetV1> for StateOverrideSet {
	fn from(value: StateOverrideSetV1) -> Self {
		Self(
			value
				.0
				.into_iter()
				.map(|(address, overrides)| (address, overrides.into()))
				.collect(),
		)
	}
}

/// Specifies how an account's storage should be overridden during a simulated call.
///
/// The Geth state override specification mandates that `state` and `stateDiff` are mutually
/// exclusive. This enum encodes that constraint at the type level.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum StorageOverride {
	/// Completely replaces the account's storage with the provided mapping. Any existing slots
	/// not present in the mapping are effectively zeroed out.
	State(BTreeMap<H256, H256>),
	/// Patches individual storage slots without affecting the rest of the account's storage.
	/// Only the specified slots are modified; all other existing slots remain unchanged.
	StateDiff(BTreeMap<H256, H256>),
}

impl From<StorageOverrideV1> for StorageOverride {
	fn from(value: StorageOverrideV1) -> Self {
		match value {
			StorageOverrideV1::State(state) => Self::State(state),
			StorageOverrideV1::StateDiff(state_diff) => Self::StateDiff(state_diff),
		}
	}
}

/// Per-account state overrides applied during `eth_call` and similar simulation methods.
///
/// All fields are optional. Only the fields that are set will be overridden; the rest of the
/// account's state is read from the chain as normal.
///
/// Conforms to the [Geth state override object specification](https://geth.ethereum.org/docs/interacting-with-geth/rpc/objects#state-override-set).
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct StateOverride {
	/// Fake balance to set for the account before executing the call.
	pub balance: Option<U256>,
	/// Fake nonce to set for the account before executing the call.
	pub nonce: Option<U256>,
	/// Fake EVM bytecode to inject into the account before executing the call.
	pub code: Option<Bytes>,
	/// Storage override specifying either a full replacement or a partial diff. These two modes
	/// are mutually exclusive per the Geth specification.
	pub storage: Option<StorageOverride>,
	/// Moves the precompile at the account's address to the specified address. Useful for
	/// overriding a precompile's code with custom logic while still being able to invoke the
	/// original precompile at a different address.
	pub move_precompile_to_address: Option<Address>,
}

impl From<StateOverrideV1> for StateOverride {
	fn from(value: StateOverrideV1) -> Self {
		Self {
			balance: value.balance,
			nonce: value.nonce,
			code: value.code,
			storage: value.storage.map(Into::into),
			move_precompile_to_address: value.move_precompile_to_address,
		}
	}
}
