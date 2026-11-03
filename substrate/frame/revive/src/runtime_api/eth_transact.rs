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

use pallet_revive_types::runtime_api::*;

use crate::{
	EthTransactInfo,
	evm::{GenericTransaction, StateOverrideSet},
};

pub struct TransactInputPayload<Moment> {
	pub tx: GenericTransaction,
	pub timestamp_override: Option<Moment>,
	pub perform_balance_checks: bool,
	pub state_overrides: Option<StateOverrideSet>,
}

impl<Moment> From<TransactVersionedInputPayload<Moment>> for TransactInputPayload<Moment> {
	fn from(value: TransactVersionedInputPayload<Moment>) -> Self {
		match value {
			TransactVersionedInputPayload::V1(payload) => payload.into(),
		}
	}
}

impl<Moment> From<TransactInputPayloadV1<Moment>> for TransactInputPayload<Moment> {
	fn from(value: TransactInputPayloadV1<Moment>) -> Self {
		Self {
			tx: value.tx.into(),
			timestamp_override: value.timestamp_override,
			perform_balance_checks: value.perform_balance_checks,
			state_overrides: value.state_overrides.map(Into::into),
		}
	}
}

pub struct TransactOutputPayload<Balance> {
	pub transact_info: EthTransactInfo<Balance>,
}

impl<Balance> From<TransactOutputPayload<Balance>> for TransactOutputPayloadV1<Balance> {
	fn from(value: TransactOutputPayload<Balance>) -> Self {
		Self { transact_info: value.transact_info.into() }
	}
}
