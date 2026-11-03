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

pub mod dap_helpers;
pub mod impls;
pub mod macros;
pub mod pools;
pub mod xcm_helpers;

pub use xcm_emulator;
pub use xcm_simulator;

// Substrate
use frame_support::parameter_types;
use sc_consensus_grandpa::AuthorityId as GrandpaId;
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_babe::AuthorityId as BabeId;
use sp_consensus_beefy::ecdsa_crypto::AuthorityId as BeefyId;
use sp_core::storage::Storage;
use sp_keyring::{Ed25519Keyring, Sr25519Keyring};
use sp_runtime::{traits::AccountIdConversion, BuildStorage};

// Polkadot
use parachains_common::BlockNumber;
use polkadot_parachain_primitives::primitives::Sibling;
use polkadot_runtime_parachains::configuration::HostConfiguration;

// Cumulus
use parachains_common::{AccountId, AuraId};
use polkadot_primitives::{AssignmentId, ValidatorId};
use xcm::v5::{Junction, Location};

pub const XCM_V2: u32 = 2;
pub const XCM_V3: u32 = 3;
pub const XCM_V4: u32 = 4;
pub const XCM_V5: u32 = 5;
pub const REF_TIME_THRESHOLD: u64 = 33;
pub const PROOF_SIZE_THRESHOLD: u64 = 33;

/// The default XCM version to set in genesis config.
pub const SAFE_XCM_VERSION: u32 = xcm::prelude::XCM_VERSION;

// (trust-backed) Asset registered on AH and reserve-transferred between Parachain and AH
pub const RESERVABLE_ASSET_ID: u32 = 1;
// ForeignAsset registered on AH and teleported between Penpal and AH
pub const PEN2_TELEPORTABLE_ASSET_ID: u32 = 2;

// USDT registered on AH as (trust-backed) Asset and reserve-transferred between Parachain and AH
pub const USDT_ID: u32 = 1984;

pub const PENPAL_A_ID: u32 = 2000;
pub const PENPAL_B_ID: u32 = 2001;
pub const ASSET_HUB_ROCOCO_ID: u32 = 1000;
pub const ASSET_HUB_WESTEND_ID: u32 = 1000;
pub const ASSETS_PALLET_ID: u8 = 50;
pub const PENPAL_ASSETS_PALLET_ID: u8 = 50;

parameter_types! {
	pub PenpalALocation: Location = Location::new(1, [Junction::Parachain(PENPAL_A_ID)]);
	pub PenpalBLocation: Location = Location::new(1, [Junction::Parachain(PENPAL_B_ID)]);
	pub PenpalAPen2TeleportableAssetLocation: Location
		= Location::new(1, [
				Junction::Parachain(PENPAL_A_ID),
				Junction::PalletInstance(PENPAL_ASSETS_PALLET_ID),
				Junction::GeneralIndex(PEN2_TELEPORTABLE_ASSET_ID.into()),
			]
		);
	pub PenpalBPen2TeleportableAssetLocation: Location
		= Location::new(1, [
				Junction::Parachain(PENPAL_B_ID),
				Junction::PalletInstance(PENPAL_ASSETS_PALLET_ID),
				Junction::GeneralIndex(PEN2_TELEPORTABLE_ASSET_ID.into()),
			]
		);
	pub PenpalASiblingSovereignAccount: AccountId = Sibling::from(PENPAL_A_ID).into_account_truncating();
	pub PenpalBSiblingSovereignAccount: AccountId = Sibling::from(PENPAL_B_ID).into_account_truncating();
}

pub fn local_penpal_asset(id: u32) -> Location {
	Location::new(
		0,
		[Junction::PalletInstance(PENPAL_ASSETS_PALLET_ID), Junction::GeneralIndex(id.into())],
	)
}

pub fn get_host_config() -> HostConfiguration<BlockNumber> {
	HostConfiguration {
		max_upward_queue_count: 10,
		max_upward_queue_size: 51200,
		max_upward_message_size: 51200,
		max_upward_message_num_per_candidate: 10,
		max_downward_message_size: 51200,
		hrmp_sender_deposit: 0,
		hrmp_recipient_deposit: 0,
		hrmp_channel_max_capacity: 1000,
		hrmp_channel_max_message_size: 102400,
		hrmp_channel_max_total_size: 102400,
		hrmp_max_parachain_outbound_channels: 30,
		hrmp_max_parachain_inbound_channels: 30,
		..Default::default()
	}
}

/// Helper function used in tests to build the genesis storage using given RuntimeGenesisConfig and
/// code Used in `legacy_vs_json_check` submods to verify storage building with JSON patch against
/// building with RuntimeGenesisConfig struct.
pub fn build_genesis_storage(builder: &dyn BuildStorage, code: &[u8]) -> Storage {
	let mut storage = builder.build_storage().unwrap();
	storage
		.top
		.insert(sp_core::storage::well_known_keys::CODE.to_vec(), code.into());
	storage
}

pub mod accounts {
	use super::*;
	pub const ALICE: &str = "Alice";
	pub const BOB: &str = "Bob";
	pub const DUMMY_EMPTY: &str = "JohnDoe";

	pub fn init_balances() -> Vec<AccountId> {
		Sr25519Keyring::well_known().map(|k| k.to_account_id()).collect()
	}
}

pub mod collators {
	use super::*;

	pub fn invulnerables() -> Vec<(AccountId, AuraId)> {
		vec![
			(Sr25519Keyring::Dave.to_account_id(), Sr25519Keyring::Dave.public().into()),
			(Sr25519Keyring::Eve.to_account_id(), Sr25519Keyring::Eve.public().into()),
		]
	}
}

pub mod validators {
	use sp_consensus_beefy::test_utils::Keyring;

	use super::*;

	pub fn initial_authorities() -> Vec<(
		AccountId,
		AccountId,
		BabeId,
		GrandpaId,
		ValidatorId,
		AssignmentId,
		AuthorityDiscoveryId,
		BeefyId,
	)> {
		vec![(
			Sr25519Keyring::AliceStash.to_account_id(),
			Sr25519Keyring::Alice.to_account_id(),
			BabeId::from(Sr25519Keyring::Alice.public()),
			GrandpaId::from(Ed25519Keyring::Alice.public()),
			ValidatorId::from(Sr25519Keyring::Alice.public()),
			AssignmentId::from(Sr25519Keyring::Alice.public()),
			AuthorityDiscoveryId::from(Sr25519Keyring::Alice.public()),
			BeefyId::from(Keyring::<BeefyId>::Alice.public()),
		)]
	}
}

pub mod snowbridge {
	use hex_literal::hex;
	// Address of WETH ERC20 token contract on remote Ethereum network
	pub const WETH: [u8; 20] = hex!("fff9976782d46cc05630d1f6ebab18b2324d6b14");
	// The Ethereum network chain ID. In this case, Sepolia testnet's chain ID.
	pub const SEPOLIA_ID: u64 = 11155111;
	// The minimum balance for ether assets pre-registered in emulated tests.
	pub const ETHER_MIN_BALANCE: u128 = 1000;
}
