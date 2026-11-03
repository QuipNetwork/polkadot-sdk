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

use alloc::vec::Vec;
use codec::{Decode, Encode};
use scale_info::TypeInfo;
use sp_core::U256;
use sp_weights::Weight;

use crate::runtime_api::*;

#[derive(Clone, Eq, PartialEq, Default, Encode, Decode, Debug, TypeInfo)]
pub struct EthTransactInfoV1<Balance> {
	pub weight_required: Weight,
	pub storage_deposit: Balance,
	pub max_storage_deposit: Balance,
	pub eth_gas: U256,
	pub data: Vec<u8>,
}

/// Exists for legacy reasons only: this is the configuration type of the unversioned
/// `eth_transact_with_config` and `eth_estimate_gas` runtime API functions and is not used anywhere
/// in the versioned runtime API.
#[derive(Debug, Encode, TypeInfo, Clone)]
pub struct DryRunConfigV1<Moment> {
	pub timestamp_override: Option<Moment>,
	pub perform_balance_checks: Option<bool>,
	pub state_overrides: Option<StateOverrideSetV1>,
}

impl<Moment> Default for DryRunConfigV1<Moment> {
	fn default() -> Self {
		Self { timestamp_override: None, perform_balance_checks: Some(true), state_overrides: None }
	}
}

impl<Moment: Decode> Decode for DryRunConfigV1<Moment> {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let timestamp_override = Option::<Moment>::decode(input)?;
		let perform_balance_checks = Option::<bool>::decode(input)?;
		let state_overrides = Option::<StateOverrideSetV1>::decode(input).unwrap_or_default();
		Ok(Self { timestamp_override, perform_balance_checks, state_overrides })
	}
}
