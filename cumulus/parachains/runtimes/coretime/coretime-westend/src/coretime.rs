// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

use crate::{xcm_config::LocationToAccountId, *};
use codec::{Decode, Encode};
use cumulus_pallet_parachain_system::RelaychainDataProvider;
use cumulus_primitives_core::relay_chain;
use frame_support::parameter_types;
use pallet_broker::{
	CoreAssignment, CoreIndex, CoretimeInterface, PartsOf57600, RCBlockNumberOf, TaskId,
};
use parachains_common::{AccountId, Balance};
use sp_runtime::traits::MaybeConvert;
use westend_runtime_constants::system_parachain::coretime;
use xcm::latest::prelude::*;
use xcm_executor::traits::ConvertLocation;

/// A type containing the encoding of the coretime pallet in the Relay chain runtime. Used to
/// construct any remote calls. The codec index must correspond to the index of `Coretime` in the
/// `construct_runtime` of the Relay chain.
#[derive(Encode, Decode)]
enum RelayRuntimePallets {
	#[codec(index = 66)]
	Coretime(CoretimeProviderCalls),
}

/// Call encoding for the calls needed from the relay coretime pallet.
#[derive(Encode, Decode)]
enum CoretimeProviderCalls {
	#[codec(index = 1)]
	RequestCoreCount(CoreIndex),
	#[codec(index = 2)]
	RequestRevenueInfoAt(relay_chain::BlockNumber),
	#[codec(index = 3)]
	CreditAccount(AccountId, Balance),
	#[codec(index = 4)]
	AssignCore(
		CoreIndex,
		relay_chain::BlockNumber,
		Vec<(CoreAssignment, PartsOf57600)>,
		Option<relay_chain::BlockNumber>,
	),
}

parameter_types! {
	pub const BrokerPalletId: PalletId = PalletId(*b"py/broke");
	pub const MinimumCreditPurchase: Balance = UNITS / 10;
	pub const MinimumEndPrice: Balance = UNITS;
}

/// Type that implements the `CoretimeInterface` for the allocation of Coretime. Meant to operate
/// from the parachain context. That is, the parachain provides a market (broker) for the sale of
/// coretime, but assumes a `CoretimeProvider` (i.e. a Relay Chain) to actually provide cores.
pub struct CoretimeAllocator;
impl CoretimeInterface for CoretimeAllocator {
	type AccountId = AccountId;
	type Balance = Balance;
	type RelayChainBlockNumberProvider = RelaychainDataProvider<Runtime>;

	fn request_core_count(count: CoreIndex) {
		use crate::coretime::CoretimeProviderCalls::RequestCoreCount;
		let request_core_count_call = RelayRuntimePallets::Coretime(RequestCoreCount(count));

		// Weight for `request_core_count` from westend benchmarks:
		// `ref_time` = 7889000 + (3 * 25000000) + (1 * 100000000) = 182889000
		// `proof_size` = 1636
		// Add 5% to each component and round to 2 significant figures.
		let call_weight = Weight::from_parts(190_000_000, 1700);

		let message = Xcm(vec![
			Instruction::UnpaidExecution {
				weight_limit: WeightLimit::Unlimited,
				check_origin: None,
			},
			Instruction::Transact {
				origin_kind: OriginKind::Native,
				call: request_core_count_call.encode().into(),
				fallback_max_weight: Some(call_weight),
			},
		]);

		match PolkadotXcm::send_xcm(Here, Location::parent(), message.clone()) {
			Ok(_) => tracing::debug!(
				target: "runtime::coretime",
				"Request to update schedulable cores sent successfully."
			),
			Err(e) => tracing::error!(
				target: "runtime::coretime", error=?e,
				"Failed to send request to update schedulable cores"
			),
		}
	}

	fn request_revenue_info_at(when: RCBlockNumberOf<Self>) {
		use crate::coretime::CoretimeProviderCalls::RequestRevenueInfoAt;
		let request_revenue_info_at_call =
			RelayRuntimePallets::Coretime(RequestRevenueInfoAt(when));

		let message = Xcm(vec![
			Instruction::UnpaidExecution {
				weight_limit: WeightLimit::Unlimited,
				check_origin: None,
			},
			Instruction::Transact {
				origin_kind: OriginKind::Native,
				call: request_revenue_info_at_call.encode().into(),
				fallback_max_weight: Some(Weight::from_parts(1_000_000_000, 200_000)),
			},
		]);

		match PolkadotXcm::send_xcm(Here, Location::parent(), message.clone()) {
			Ok(_) => tracing::debug!(
				target: "runtime::coretime",
				"Request for revenue information sent successfully."
			),
			Err(e) => tracing::error!(
				target: "runtime::coretime", error=?e,
				"Request for revenue information failed to send"
			),
		}
	}

	fn credit_account(who: Self::AccountId, amount: Self::Balance) {
		use crate::coretime::CoretimeProviderCalls::CreditAccount;
		let credit_account_call = RelayRuntimePallets::Coretime(CreditAccount(who, amount));

		let message = Xcm(vec![
			Instruction::UnpaidExecution {
				weight_limit: WeightLimit::Unlimited,
				check_origin: None,
			},
			Instruction::Transact {
				origin_kind: OriginKind::Native,
				call: credit_account_call.encode().into(),
				fallback_max_weight: Some(Weight::from_parts(1_000_000_000, 200_000)),
			},
		]);

		match PolkadotXcm::send_xcm(Here, Location::parent(), message.clone()) {
			Ok(_) => tracing::debug!(
				target: "runtime::coretime",
				"Instruction to credit account sent successfully."
			),
			Err(e) => tracing::error!(
				target: "runtime::coretime", error=?e,
				"Instruction to credit account failed to send"
			),
		}
	}

	fn assign_core(
		core: CoreIndex,
		begin: RCBlockNumberOf<Self>,
		assignment: Vec<(CoreAssignment, PartsOf57600)>,
		end_hint: Option<RCBlockNumberOf<Self>>,
	) {
		use crate::coretime::CoretimeProviderCalls::AssignCore;

		// Weight for `assign_core` from westend benchmarks:
		// `ref_time` = 10177115 + (1 * 25000000) + (2 * 100000000) + (57600 * 13932) = 937660315
		// `proof_size` = 3612
		// Add 5% to each component and round to 2 significant figures.
		let call_weight = Weight::from_parts(980_000_000, 3800);

		// A maximum of 28 assignments fit in one message, so we split the assignments and send as
		// multiple messages. This will get reassembled into a full list of assignments on the
		// relay chain side.

		for chunk in assignment.chunks(28) {
			let partial_assignment = chunk.to_vec();

			let assign_core_call = RelayRuntimePallets::Coretime(AssignCore(
				core,
				begin,
				partial_assignment,
				end_hint,
			));

			let message = Xcm(vec![
				Instruction::UnpaidExecution {
					weight_limit: WeightLimit::Unlimited,
					check_origin: None,
				},
				Instruction::Transact {
					origin_kind: OriginKind::Native,
					call: assign_core_call.encode().into(),
					fallback_max_weight: Some(call_weight),
				},
			]);

			match PolkadotXcm::send_xcm(Here, Location::parent(), message) {
				Ok(_) => tracing::debug!(
					target: "runtime::coretime",
					"Core assignment sent successfully."
				),
				Err(e) => tracing::error!(
					target: "runtime::coretime", error=?e,
					"Core assignment failed to send"
				),
			}
		}
	}
}

pub struct SovereignAccountOf;
impl MaybeConvert<TaskId, AccountId> for SovereignAccountOf {
	fn maybe_convert(id: TaskId) -> Option<AccountId> {
		// Currently all tasks are parachains.
		let location = Location::new(1, [Parachain(id)]);
		LocationToAccountId::convert_location(&location)
	}
}

impl pallet_broker::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Currency = Balances;
	type OnRevenue = AccumulateForward;
	type TimeslicePeriod = ConstU32<{ coretime::TIMESLICE_PERIOD }>;
	type MaxLeasedCores = ConstU32<50>;
	type MaxReservedCores = ConstU32<50>;
	type Coretime = CoretimeAllocator;
	type ConvertBalance = sp_runtime::traits::Identity;
	type WeightInfo = weights::pallet_broker::WeightInfo<Runtime>;
	type PalletId = BrokerPalletId;
	type AdminOrigin = EnsureRoot<AccountId>;
	type SovereignAccountOf = SovereignAccountOf;
	type MaxAutoRenewals = ConstU32<50>;
	type PriceAdapter = pallet_broker::MinimumPrice<Balance, MinimumEndPrice>;
	type MinimumCreditPurchase = MinimumCreditPurchase;
}
