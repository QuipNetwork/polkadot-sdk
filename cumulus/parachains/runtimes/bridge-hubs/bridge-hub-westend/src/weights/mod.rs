// This file is part of Cumulus.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Expose the auto generated weight files.

use ::pallet_bridge_grandpa::WeightInfoExt as GrandpaWeightInfoExt;
use ::pallet_bridge_messages::WeightInfoExt as MessagesWeightInfoExt;
use ::pallet_bridge_parachains::WeightInfoExt as ParachainsWeightInfoExt;
use ::pallet_bridge_relayers::WeightInfo as _;

pub mod block_weights;
pub mod cumulus_pallet_parachain_system;
pub mod cumulus_pallet_weight_reclaim;
pub mod cumulus_pallet_xcmp_queue;
pub mod extrinsic_weights;
pub mod frame_system;
pub mod frame_system_extensions;
pub mod pallet_accumulate_and_forward;
pub mod pallet_balances;
pub mod pallet_bridge_grandpa;
pub mod pallet_bridge_messages;
pub mod pallet_bridge_parachains;
pub mod pallet_bridge_relayers;
pub mod pallet_collator_selection;
pub mod pallet_message_queue;
pub mod pallet_multisig;
pub mod pallet_session;
pub mod pallet_timestamp;
pub mod pallet_transaction_payment;
pub mod pallet_utility;
pub mod pallet_xcm;
pub mod paritydb_weights;
pub mod rocksdb_weights;
pub mod xcm;

pub mod snowbridge_pallet_ethereum_client;
pub mod snowbridge_pallet_inbound_queue;
pub mod snowbridge_pallet_inbound_queue_v2;
pub mod snowbridge_pallet_outbound_queue;
pub mod snowbridge_pallet_outbound_queue_v2;
pub mod snowbridge_pallet_system;
pub mod snowbridge_pallet_system_v2;

pub use block_weights::constants::BlockExecutionWeight;
pub use extrinsic_weights::constants::ExtrinsicBaseWeight;
pub use rocksdb_weights::constants::RocksDbWeight;

use crate::Runtime;
use frame_support::weights::Weight;

// import trait from dependency module
use ::pallet_bridge_relayers::WeightInfoExt as _;

impl GrandpaWeightInfoExt for pallet_bridge_grandpa::WeightInfo<crate::Runtime> {
	fn submit_finality_proof_overhead_from_runtime() -> Weight {
		// our signed extension:
		// 1) checks whether relayer registration is active from validate/pre_dispatch;
		// 2) may slash and deregister relayer from post_dispatch
		// (2) includes (1), so (2) is the worst case
		pallet_bridge_relayers::WeightInfo::<Runtime>::slash_and_deregister()
	}
}

impl MessagesWeightInfoExt for pallet_bridge_messages::WeightInfo<crate::Runtime> {
	fn expected_extra_storage_proof_size() -> u32 {
		bp_bridge_hub_rococo::EXTRA_STORAGE_PROOF_SIZE
	}

	fn receive_messages_proof_overhead_from_runtime() -> Weight {
		pallet_bridge_relayers::WeightInfo::<Runtime>::receive_messages_proof_overhead_from_runtime(
		)
	}

	fn receive_messages_delivery_proof_overhead_from_runtime() -> Weight {
		pallet_bridge_relayers::WeightInfo::<Runtime>::receive_messages_delivery_proof_overhead_from_runtime()
	}
}

impl ParachainsWeightInfoExt for pallet_bridge_parachains::WeightInfo<crate::Runtime> {
	fn expected_extra_storage_proof_size() -> u32 {
		bp_bridge_hub_rococo::EXTRA_STORAGE_PROOF_SIZE
	}

	fn submit_parachain_heads_overhead_from_runtime() -> Weight {
		// our signed extension:
		// 1) checks whether relayer registration is active from validate/pre_dispatch;
		// 2) may slash and deregister relayer from post_dispatch
		// (2) includes (1), so (2) is the worst case
		pallet_bridge_relayers::WeightInfo::<Runtime>::slash_and_deregister()
	}
}
