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

//! Bridge definitions that can be used by multiple BridgeHub flavors.
//! All configurations here should be dedicated to a single chain; in other words, we don't need two
//! chains for a single pallet configuration.
//!
//! For example, the messaging pallet needs to know the sending and receiving chains, but the
//! GRANDPA tracking pallet only needs to be aware of one chain.

use super::{weights, AccountId, Balance, Balances, BlockNumber, Runtime, RuntimeEvent};
use bp_parachains::SingleParaStoredHeaderDataBuilder;
use bp_relayers::RewardsAccountParams;
use frame_support::{parameter_types, traits::ConstU32};

parameter_types! {
	pub const RelayChainHeadersToKeep: u32 = 1024;
	pub const ParachainHeadsToKeep: u32 = 64;

	pub const WestendBridgeParachainPalletName: &'static str = bp_westend::PARAS_PALLET_NAME;
	pub const MaxWestendParaHeadDataSize: u32 = bp_westend::MAX_NESTED_PARACHAIN_HEAD_DATA_SIZE;

	pub storage RequiredStakeForStakeAndSlash: Balance = 1_000_000;
	pub const RelayerStakeLease: u32 = 8;
	pub const RelayerStakeReserveId: [u8; 8] = *b"brdgrlrs";

	pub storage DeliveryRewardInBalance: u64 = 1_000_000;
}

/// Add GRANDPA bridge pallet to track Westend relay chain.
pub type BridgeGrandpaWestendInstance = pallet_bridge_grandpa::Instance3;
impl pallet_bridge_grandpa::Config<BridgeGrandpaWestendInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type BridgedChain = bp_westend::Westend;
	type MaxFreeHeadersPerBlock = ConstU32<4>;
	type FreeHeadersInterval = ConstU32<5>;
	type HeadersToKeep = RelayChainHeadersToKeep;
	type WeightInfo = weights::pallet_bridge_grandpa::WeightInfo<Runtime>;
}

/// Add parachain bridge pallet to track Westend BridgeHub parachain
pub type BridgeParachainWestendInstance = pallet_bridge_parachains::Instance3;
impl pallet_bridge_parachains::Config<BridgeParachainWestendInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type WeightInfo = weights::pallet_bridge_parachains::WeightInfo<Runtime>;
	type BridgesGrandpaPalletInstance = BridgeGrandpaWestendInstance;
	type ParasPalletName = WestendBridgeParachainPalletName;
	type ParaStoredHeaderDataBuilder =
		SingleParaStoredHeaderDataBuilder<bp_bridge_hub_westend::BridgeHubWestend>;
	type HeadsToKeep = ParachainHeadsToKeep;
	type MaxParaHeadDataSize = MaxWestendParaHeadDataSize;
	type OnNewHead = ();
}

/// Allows collect and claim rewards for relayers
pub type RelayersForLegacyLaneIdsMessagesInstance = ();
impl pallet_bridge_relayers::Config<RelayersForLegacyLaneIdsMessagesInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RewardBalance = Balance;
	type Reward = RewardsAccountParams<bp_messages::LegacyLaneId>;
	type PaymentProcedure = bp_relayers::PayRewardFromAccount<
		pallet_balances::Pallet<Runtime>,
		AccountId,
		bp_messages::LegacyLaneId,
		Self::RewardBalance,
	>;
	type StakeAndSlash = pallet_bridge_relayers::StakeAndSlashNamed<
		AccountId,
		BlockNumber,
		Balances,
		RelayerStakeReserveId,
		RequiredStakeForStakeAndSlash,
		RelayerStakeLease,
	>;
	type Balance = Balance;
	type WeightInfo = weights::pallet_bridge_relayers_legacy::WeightInfo<Runtime>;
}

/// Allows collect and claim rewards for relayers
pub type RelayersForPermissionlessLanesInstance = pallet_bridge_relayers::Instance2;
impl pallet_bridge_relayers::Config<RelayersForPermissionlessLanesInstance> for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RewardBalance = Balance;
	type Reward = RewardsAccountParams<bp_messages::HashedLaneId>;
	type PaymentProcedure = bp_relayers::PayRewardFromAccount<
		pallet_balances::Pallet<Runtime>,
		AccountId,
		bp_messages::HashedLaneId,
		Self::RewardBalance,
	>;
	type StakeAndSlash = pallet_bridge_relayers::StakeAndSlashNamed<
		AccountId,
		BlockNumber,
		Balances,
		RelayerStakeReserveId,
		RequiredStakeForStakeAndSlash,
		RelayerStakeLease,
	>;
	type Balance = Balance;
	type WeightInfo = weights::pallet_bridge_relayers_permissionless_lanes::WeightInfo<Runtime>;
}
