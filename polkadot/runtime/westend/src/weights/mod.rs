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

//! A list of the different weight modules for our runtime.

pub mod frame_system;
pub mod frame_system_extensions;
pub mod pallet_accumulate_and_forward;
pub mod pallet_asset_rate;
pub mod pallet_balances;
pub mod pallet_beefy_mmr;
pub mod pallet_identity;
pub mod pallet_indices;
pub mod pallet_message_queue;
pub mod pallet_migrations;
pub mod pallet_mmr;
pub mod pallet_multisig;
pub mod pallet_parameters;
pub mod pallet_preimage;
pub mod pallet_proxy;
pub mod pallet_scheduler;
pub mod pallet_session;
pub mod pallet_sudo;
pub mod pallet_timestamp;
pub mod pallet_transaction_payment;
pub mod pallet_utility;
pub mod pallet_vesting;
pub mod pallet_xcm;
pub mod polkadot_runtime_common_assigned_slots;
pub mod polkadot_runtime_common_auctions;
pub mod polkadot_runtime_common_crowdloan;
pub mod polkadot_runtime_common_identity_migrator;
pub mod polkadot_runtime_common_paras_registrar;
pub mod polkadot_runtime_common_slots;
pub mod polkadot_runtime_parachains_configuration;
pub mod polkadot_runtime_parachains_coretime;
pub mod polkadot_runtime_parachains_disputes;
pub mod polkadot_runtime_parachains_disputes_slashing;
pub mod polkadot_runtime_parachains_hrmp;
pub mod polkadot_runtime_parachains_inclusion;
pub mod polkadot_runtime_parachains_initializer;
pub mod polkadot_runtime_parachains_on_demand;
pub mod polkadot_runtime_parachains_paras;
pub mod polkadot_runtime_parachains_paras_inherent;
pub mod xcm;
