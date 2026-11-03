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

//! Genesis Presets for the Kitchensink Runtime

use polkadot_sdk::*;

use crate::{
	constants::currency::*, frame_support::build_struct_json_patch, AccountId, AssetsConfig,
	BabeConfig, Balance, BalancesConfig, ElectionsConfig, NominationPoolsConfig, ReviveConfig,
	RuntimeGenesisConfig, SessionConfig, SessionKeys, SocietyConfig, StakerStatus, StakingConfig,
	SudoConfig, TechnicalCommitteeConfig, BABE_GENESIS_EPOCH_CONFIG,
};
use alloc::{vec, vec::Vec};
use pallet_im_online::sr25519::AuthorityId as ImOnlineId;
use pallet_revive::AddressMapper;
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_babe::AuthorityId as BabeId;
use sp_consensus_beefy::ecdsa_crypto::AuthorityId as BeefyId;
use sp_consensus_grandpa::AuthorityId as GrandpaId;
use sp_core::{crypto::get_public_from_string_or_panic, sr25519, Get};
use sp_genesis_builder::PresetId;
use sp_keyring::Sr25519Keyring;
use sp_mixnet::types::AuthorityId as MixnetId;
use sp_runtime::Perbill;

pub const ENDOWMENT: Balance = 10_000_000 * DOLLARS;
pub const STASH: Balance = ENDOWMENT / 1000;

/// The staker type as supplied ot the Staking config.
pub type Staker = (AccountId, AccountId, Balance, StakerStatus<AccountId>);

/// Helper function to create RuntimeGenesisConfig json patch for testing.
pub fn kitchensink_genesis(
	initial_authorities: Vec<(AccountId, AccountId, SessionKeys)>,
	root_key: AccountId,
	endowed_accounts: Vec<AccountId>,
	stakers: Vec<Staker>,
) -> serde_json::Value {
	let validator_count = initial_authorities.len() as u32;
	let minimum_validator_count = validator_count;

	let collective = collective(&endowed_accounts);

	build_struct_json_patch!(RuntimeGenesisConfig {
		balances: BalancesConfig {
			balances: endowed_accounts.iter().cloned().map(|x| (x, ENDOWMENT)).collect(),
			..Default::default()
		},
		session: SessionConfig {
			keys: initial_authorities
				.iter()
				.map(|x| { (x.0.clone(), x.1.clone(), x.2.clone()) })
				.collect(),
		},
		staking: StakingConfig {
			validator_count,
			minimum_validator_count,
			invulnerables: initial_authorities
				.iter()
				.map(|x| x.0.clone())
				.collect::<Vec<_>>()
				.try_into()
				.expect("Too many invulnerable validators: upper limit is MaxInvulnerables from pallet staking config"),
			slash_reward_fraction: Perbill::from_percent(10),
			stakers,
		},
		elections: ElectionsConfig {
			members: collective.iter().cloned().map(|member| (member, STASH)).collect(),
		},
		technical_committee: TechnicalCommitteeConfig { members: collective },
		sudo: SudoConfig { key: Some(root_key) },
		babe: BabeConfig { epoch_config: BABE_GENESIS_EPOCH_CONFIG },
		society: SocietyConfig { pot: 0 },
		assets: AssetsConfig {
			// This asset is used by the NIS pallet as counterpart currency.
			assets: vec![(9, Sr25519Keyring::Alice.to_account_id(), true, 1)],
			..Default::default()
		},
		nomination_pools: NominationPoolsConfig {
			min_create_bond: 10 * DOLLARS,
			min_join_bond: 1 * DOLLARS,
		},
		revive: ReviveConfig {
			mapped_accounts: if <crate::Runtime as pallet_revive::Config>::AutoMap::get() {
				vec![]
			} else {
				endowed_accounts
					.iter()
					.filter(|x| {
						!<crate::Runtime as pallet_revive::Config>::AddressMapper::is_eth_derived(x)
					})
					.cloned()
					.collect()
			},
		},
	})
}

/// Provides the JSON representation of predefined genesis config for given `id`.
pub fn get_preset(id: &PresetId) -> Option<Vec<u8>> {
	// Note: Can't use `Sr25519Keyring::Alice.to_seed()` because the seed comes with `//`.
	let (alice_stash, alice, alice_session_keys) = authority_keys_from_seed("Alice");
	let (bob_stash, _bob, bob_session_keys) = authority_keys_from_seed("Bob");

	let endowed = well_known_including_eth_accounts();

	let patch = match id.as_ref() {
		sp_genesis_builder::DEV_RUNTIME_PRESET => kitchensink_genesis(
			// Use stash as controller account, otherwise grandpa can't load the authority set at
			// genesis.
			vec![(alice_stash.clone(), alice_stash.clone(), alice_session_keys)],
			alice.clone(),
			endowed,
			vec![validator(alice_stash.clone())],
		),
		sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET => kitchensink_genesis(
			vec![
				// Use stash as controller account, otherwise grandpa can't load the authority set
				// at genesis.
				(alice_stash.clone(), alice_stash.clone(), alice_session_keys),
				(bob_stash.clone(), bob_stash.clone(), bob_session_keys),
			],
			alice,
			endowed,
			vec![validator(alice_stash), validator(bob_stash)],
		),
		_ => return None,
	};

	Some(
		serde_json::to_string(&patch)
			.expect("serialization to json is expected to work. qed.")
			.into_bytes(),
	)
}

/// List of supported presets.
pub fn preset_names() -> Vec<PresetId> {
	vec![
		PresetId::from(sp_genesis_builder::DEV_RUNTIME_PRESET),
		PresetId::from(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET),
	]
}

/// Sets up the `account` to be a staker of validator variant as supplied to the
/// staking config.
pub fn validator(account: AccountId) -> Staker {
	// validator, controller, stash, staker status
	(account.clone(), account, STASH, StakerStatus::Validator)
}

/// Extract some accounts from endowed to be put into the collective.
fn collective(endowed: &[AccountId]) -> Vec<AccountId> {
	const MAX_COLLECTIVE_SIZE: usize = 50;
	let endowed_accounts_count = endowed.len();
	endowed
		.iter()
		.take((endowed_accounts_count.div_ceil(2)).min(MAX_COLLECTIVE_SIZE))
		.cloned()
		.collect()
}

/// The Keyring's wellknown accounts + Alith and Baltathar.
///
/// Some integration tests require these ETH accounts.
pub fn well_known_including_eth_accounts() -> Vec<AccountId> {
	Sr25519Keyring::well_known()
		.map(|k| k.to_account_id())
		.chain([
			// subxt_signer::eth::dev::alith()
			array_bytes::hex_n_into_unchecked(
				"f24ff3a9cf04c71dbc94d0b566f7a27b94566caceeeeeeeeeeeeeeeeeeeeeeee",
			),
			// subxt_signer::eth::dev::baltathar()
			array_bytes::hex_n_into_unchecked(
				"3cd0a705a2dc65e5b1e1205896baa2be8a07c6e0eeeeeeeeeeeeeeeeeeeeeeee",
			),
		])
		.collect::<Vec<_>>()
}

/// Helper function to generate stash, controller and session key from seed.
///
/// Note: `//` is prepended internally.
pub fn authority_keys_from_seed(seed: &str) -> (AccountId, AccountId, SessionKeys) {
	(
		get_public_from_string_or_panic::<sr25519::Public>(&alloc::format!("{seed}//stash")).into(),
		get_public_from_string_or_panic::<sr25519::Public>(seed).into(),
		session_keys_from_seed(seed),
	)
}

pub fn session_keys(
	grandpa: GrandpaId,
	babe: BabeId,
	im_online: ImOnlineId,
	authority_discovery: AuthorityDiscoveryId,
	mixnet: MixnetId,
	beefy: BeefyId,
) -> SessionKeys {
	SessionKeys { grandpa, babe, im_online, authority_discovery, mixnet, beefy }
}

/// We have this method as there is no straight forward way to convert the
/// account keyring into these ids.
///
/// Note: `//` is prepended internally.
pub fn session_keys_from_seed(seed: &str) -> SessionKeys {
	session_keys(
		get_public_from_string_or_panic::<GrandpaId>(seed),
		get_public_from_string_or_panic::<BabeId>(seed),
		get_public_from_string_or_panic::<ImOnlineId>(seed),
		get_public_from_string_or_panic::<AuthorityDiscoveryId>(seed),
		get_public_from_string_or_panic::<MixnetId>(seed),
		get_public_from_string_or_panic::<BeefyId>(seed),
	)
}
