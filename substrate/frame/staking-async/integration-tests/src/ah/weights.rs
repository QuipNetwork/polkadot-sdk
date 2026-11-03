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

//! Weights to be used with `full_election_cycle_with_occasional_out_of_weight_completes` test.
//!
//! Note: we put as many things as possible as `unreachable!()` to limit the scope.

use frame::deps::frame_system;
use frame_election_provider_support::Weight;
use frame_support::traits::Get;
use pallet_election_provider_multi_block::weights::traits::{
	pallet_election_provider_multi_block_signed, pallet_election_provider_multi_block_unsigned,
	pallet_election_provider_multi_block_verifier,
};

use crate::ah;

pub const SMALL: Weight = Weight::from_parts(10, 0);
pub const MEDIUM: Weight = Weight::from_parts(100, 0);
pub const LARGE: Weight = Weight::from_parts(1_000, 0);

pub struct MultiBlockElectionWeightInfo;
impl pallet_election_provider_multi_block::WeightInfo for MultiBlockElectionWeightInfo {
	fn admin_set() -> Weight {
		unreachable!()
	}
	fn manage_fallback() -> Weight {
		unreachable!()
	}
	fn export_non_terminal() -> Weight {
		MEDIUM
	}
	fn export_terminal() -> Weight {
		LARGE
	}
	fn per_block_nothing() -> Weight {
		SMALL
	}
	fn per_block_snapshot_msp() -> Weight {
		LARGE
	}
	fn per_block_snapshot_rest() -> Weight {
		MEDIUM
	}
	fn per_block_start_signed_validation() -> Weight {
		SMALL
	}
}

impl pallet_election_provider_multi_block_verifier::WeightInfo for MultiBlockElectionWeightInfo {
	fn verification_invalid_non_terminal(_: u32) -> Weight {
		MEDIUM
	}
	fn verification_invalid_terminal() -> Weight {
		LARGE
	}
	fn verification_valid_non_terminal() -> Weight {
		MEDIUM
	}
	fn verification_valid_terminal() -> Weight {
		LARGE
	}
}

impl pallet_election_provider_multi_block_signed::WeightInfo for MultiBlockElectionWeightInfo {
	fn bail() -> Weight {
		unreachable!()
	}
	fn clear_old_round_data(_: u32) -> Weight {
		unreachable!()
	}
	fn register_eject() -> Weight {
		Default::default()
	}
	fn register_not_full() -> Weight {
		// we submit pages in tests
		Default::default()
	}
	fn submit_page() -> Weight {
		Default::default()
	}
	fn unset_page() -> Weight {
		unreachable!()
	}
	fn claim_unpaid_reward() -> Weight {
		unreachable!()
	}
}

impl pallet_election_provider_multi_block_unsigned::WeightInfo for MultiBlockElectionWeightInfo {
	fn mine_solution(_p: u32) -> Weight {
		unreachable!()
	}
	fn submit_unsigned() -> Weight {
		// NOTE: this one is checked in the integrity tests of the runtime, we don't care about it
		// here.
		Default::default()
	}
	fn validate_unsigned() -> Weight {
		unreachable!()
	}
}

pub struct StakingAsyncWeightInfo;
impl pallet_staking_async::WeightInfo for StakingAsyncWeightInfo {
	fn bond() -> Weight {
		unreachable!()
	}
	fn bond_extra() -> Weight {
		unreachable!()
	}
	fn unbond() -> Weight {
		unreachable!()
	}
	fn withdraw_unbonded_update() -> Weight {
		unreachable!()
	}
	fn withdraw_unbonded_kill() -> Weight {
		unreachable!()
	}
	fn validate() -> Weight {
		unreachable!()
	}
	fn kick(_: u32) -> Weight {
		unreachable!()
	}
	fn nominate(_: u32) -> Weight {
		unreachable!()
	}
	fn chill() -> Weight {
		unreachable!()
	}
	fn set_payee() -> Weight {
		unreachable!()
	}
	fn update_payee() -> Weight {
		unreachable!()
	}
	fn set_controller() -> Weight {
		unreachable!()
	}
	fn set_validator_count() -> Weight {
		unreachable!()
	}
	fn force_no_eras() -> Weight {
		unreachable!()
	}
	fn force_new_era() -> Weight {
		unreachable!()
	}
	fn force_new_era_always() -> Weight {
		unreachable!()
	}
	fn deprecate_controller_batch(_: u32) -> Weight {
		unreachable!()
	}
	fn force_unstake() -> Weight {
		unreachable!()
	}
	fn cancel_deferred_slash(_: u32) -> Weight {
		unreachable!()
	}
	fn payout_stakers_alive_staked(_: u32) -> Weight {
		Weight::default()
	}
	fn rebond(_: u32) -> Weight {
		unreachable!()
	}
	fn reap_stash() -> Weight {
		unreachable!()
	}
	fn set_staking_configs_all_set() -> Weight {
		unreachable!()
	}
	fn set_staking_configs_all_remove() -> Weight {
		unreachable!()
	}
	fn chill_other() -> Weight {
		unreachable!()
	}
	fn force_apply_min_commission() -> Weight {
		unreachable!()
	}
	fn set_min_commission() -> Weight {
		unreachable!()
	}
	fn set_max_commission() -> Weight {
		unreachable!()
	}
	fn set_validator_self_stake_incentive_config() -> Weight {
		unreachable!()
	}
	fn prune_era_validator_incentive_weight(_v: u32) -> Weight {
		unreachable!()
	}
	fn restore_ledger() -> Weight {
		unreachable!()
	}
	fn migrate_currency() -> Weight {
		unreachable!()
	}
	fn apply_slash(_n: u32) -> Weight {
		Default::default()
	}
	fn process_offence_queue() -> Weight {
		let max_block =
			<ah::mock::BlockWeights as Get<frame_system::limits::BlockWeights>>::get().max_block;
		// Reserve headroom for DAP drip weight which also runs in on_initialize.
		max_block.saturating_sub(<() as pallet_dap::WeightInfo>::drip_issuance())
	}
	fn rc_on_offence(_: u32) -> Weight {
		Default::default()
	}
	fn rc_on_session_report(_: u32) -> Weight {
		Default::default()
	}
	fn prune_era_stakers_paged(_: u32) -> Weight {
		unreachable!()
	}
	fn prune_era_stakers_overview(_: u32) -> Weight {
		unreachable!()
	}
	fn prune_era_validator_prefs(_: u32) -> Weight {
		unreachable!()
	}
	fn prune_era_claimed_rewards(_: u32) -> Weight {
		unreachable!()
	}
	fn prune_era_validator_reward() -> Weight {
		unreachable!()
	}
	fn prune_era_reward_points() -> Weight {
		unreachable!()
	}
	fn prune_era_single_entry_cleanups() -> Weight {
		unreachable!()
	}
	fn prune_era_validator_slash_in_era(_: u32) -> Weight {
		unreachable!()
	}
	fn chill_inactive(_: u32) -> Weight {
		unreachable!()
	}
}
