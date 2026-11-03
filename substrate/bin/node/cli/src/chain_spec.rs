// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! Substrate chain configurations.

use polkadot_sdk::*;

use crate::chain_spec::sc_service::Properties;
use kitchensink_runtime::{
	genesis_config_presets::{Staker, ENDOWMENT, STASH},
	wasm_binary_unwrap, Block, MaxNominations, StakerStatus,
};
use pallet_im_online::sr25519::AuthorityId as ImOnlineId;
use pallet_revive::AddressMapper;
use sc_chain_spec::ChainSpecExtension;
use sc_service::ChainType;
use sc_telemetry::TelemetryEndpoints;
use serde::{Deserialize, Serialize};
use sp_authority_discovery::AuthorityId as AuthorityDiscoveryId;
use sp_consensus_babe::AuthorityId as BabeId;
use sp_consensus_beefy::ecdsa_crypto::AuthorityId as BeefyId;
use sp_consensus_grandpa::AuthorityId as GrandpaId;
use sp_core::{crypto::UncheckedInto, Get};
use sp_mixnet::types::AuthorityId as MixnetId;

pub use kitchensink_runtime::RuntimeGenesisConfig;
pub use node_primitives::{AccountId, Balance, Signature};

const STAGING_TELEMETRY_URL: &str = "wss://telemetry.polkadot.io/submit/";

/// Node `ChainSpec` extensions.
///
/// Additional parameters for some Substrate core modules,
/// customizable from the chain spec.
#[derive(Default, Clone, Serialize, Deserialize, ChainSpecExtension)]
#[serde(rename_all = "camelCase")]
pub struct Extensions {
	/// Block numbers with known hashes.
	pub fork_blocks: sc_client_api::ForkBlocks<Block>,
	/// Known bad block hashes.
	pub bad_blocks: sc_client_api::BadBlocks<Block>,
	/// The light sync state extension used by the sync-state rpc.
	pub light_sync_state: sc_sync_state_rpc::LightSyncStateExtension,
}

/// Specialized `ChainSpec`.
pub type ChainSpec = sc_service::GenericChainSpec<Extensions>;
/// Flaming Fir testnet generator
pub fn flaming_fir_config() -> Result<ChainSpec, String> {
	ChainSpec::from_json_bytes(&include_bytes!("../res/flaming-fir.json")[..])
}

fn configure_accounts_for_staging_testnet() -> (
	Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		MixnetId,
		BeefyId,
	)>,
	AccountId,
	Vec<AccountId>,
) {
	#[rustfmt::skip]
	// stash, controller, session-key, beefy id
	// generated with secret:
	// for i in 1 2 3 4 ; do for j in stash controller; do subkey inspect "$secret"/fir/$j/$i; done; done
	//
	// and
	//
	// for i in 1 2 3 4 ; do for j in session; do subkey inspect --scheme ed25519 "$secret"//fir//$j//$i; done; done
	//
	// and
	//
	// for i in 1 2 3 4 ; do for j in session; do subkey inspect --scheme ecdsa "$secret"//fir//$j//$i; done; done

	let initial_authorities: Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		MixnetId,
		BeefyId,
	)> = vec![
		(
			// 5Fbsd6WXDGiLTxunqeK5BATNiocfCqu9bS1yArVjCgeBLkVy
			array_bytes::hex_n_into_unchecked("9c7a2ee14e565db0c69f78c7b4cd839fbf52b607d867e9e9c5a79042898a0d12"),
			// 5EnCiV7wSHeNhjW3FSUwiJNkcc2SBkPLn5Nj93FmbLtBjQUq
			array_bytes::hex_n_into_unchecked("781ead1e2fa9ccb74b44c19d29cb2a7a4b5be3972927ae98cd3877523976a276"),
			// 5Fb9ayurnxnaXj56CjmyQLBiadfRCqUbL2VWNbbe1nZU6wiC
			array_bytes::hex2array_unchecked("9becad03e6dcac03cee07edebca5475314861492cdfc96a2144a67bbe9699332")
				.unchecked_into(),
			// 5EZaeQ8djPcq9pheJUhgerXQZt9YaHnMJpiHMRhwQeinqUW8
			array_bytes::hex2array_unchecked("6e7e4eb42cbd2e0ab4cae8708ce5509580b8c04d11f6758dbf686d50fe9f9106")
				.unchecked_into(),
			// 5EZaeQ8djPcq9pheJUhgerXQZt9YaHnMJpiHMRhwQeinqUW8
			array_bytes::hex2array_unchecked("6e7e4eb42cbd2e0ab4cae8708ce5509580b8c04d11f6758dbf686d50fe9f9106")
				.unchecked_into(),
			// 5EZaeQ8djPcq9pheJUhgerXQZt9YaHnMJpiHMRhwQeinqUW8
			array_bytes::hex2array_unchecked("6e7e4eb42cbd2e0ab4cae8708ce5509580b8c04d11f6758dbf686d50fe9f9106")
				.unchecked_into(),
			// 5EZaeQ8djPcq9pheJUhgerXQZt9YaHnMJpiHMRhwQeinqUW8
			array_bytes::hex2array_unchecked("6e7e4eb42cbd2e0ab4cae8708ce5509580b8c04d11f6758dbf686d50fe9f9106")
				.unchecked_into(),
			// 5DMLFcDdLLQbw696YfHaWBpQR99HwR456ycSCfr6L7KXGYK8
			array_bytes::hex2array_unchecked("035560fafa241739869360aa4b32bc98953172ceb41a19c6cc1a27962fb3d1ecec")
				.unchecked_into(),
		),
		(
			// 5ERawXCzCWkjVq3xz1W5KGNtVx2VdefvZ62Bw1FEuZW4Vny2
			array_bytes::hex_n_into_unchecked("68655684472b743e456907b398d3a44c113f189e56d1bbfd55e889e295dfde78"),
			// 5Gc4vr42hH1uDZc93Nayk5G7i687bAQdHHc9unLuyeawHipF
			array_bytes::hex_n_into_unchecked("c8dc79e36b29395413399edaec3e20fcca7205fb19776ed8ddb25d6f427ec40e"),
			// 5EockCXN6YkiNCDjpqqnbcqd4ad35nU4RmA1ikM4YeRN4WcE
			array_bytes::hex2array_unchecked("7932cff431e748892fa48e10c63c17d30f80ca42e4de3921e641249cd7fa3c2f")
				.unchecked_into(),
			// 5DhLtiaQd1L1LU9jaNeeu9HJkP6eyg3BwXA7iNMzKm7qqruQ
			array_bytes::hex2array_unchecked("482dbd7297a39fa145c570552249c2ca9dd47e281f0c500c971b59c9dcdcd82e")
				.unchecked_into(),
			// 5DhLtiaQd1L1LU9jaNeeu9HJkP6eyg3BwXA7iNMzKm7qqruQ
			array_bytes::hex2array_unchecked("482dbd7297a39fa145c570552249c2ca9dd47e281f0c500c971b59c9dcdcd82e")
				.unchecked_into(),
			// 5DhLtiaQd1L1LU9jaNeeu9HJkP6eyg3BwXA7iNMzKm7qqruQ
			array_bytes::hex2array_unchecked("482dbd7297a39fa145c570552249c2ca9dd47e281f0c500c971b59c9dcdcd82e")
				.unchecked_into(),
			// 5DhLtiaQd1L1LU9jaNeeu9HJkP6eyg3BwXA7iNMzKm7qqruQ
			array_bytes::hex2array_unchecked("482dbd7297a39fa145c570552249c2ca9dd47e281f0c500c971b59c9dcdcd82e")
				.unchecked_into(),
			// 5FYk11kNtB4178wLKJ2RNoUzzcjgRUciFe3SJDVZXhqX4dzG
			array_bytes::hex2array_unchecked("02da1ab255ed888ee3e19b73d335fc13160b3eb10456c2d17c6a8ea7de403d2445")
				.unchecked_into(),
		),
		(
			// 5DyVtKWPidondEu8iHZgi6Ffv9yrJJ1NDNLom3X9cTDi98qp
			array_bytes::hex_n_into_unchecked("547ff0ab649283a7ae01dbc2eb73932eba2fb09075e9485ff369082a2ff38d65"),
			// 5FeD54vGVNpFX3PndHPXJ2MDakc462vBCD5mgtWRnWYCpZU9
			array_bytes::hex_n_into_unchecked("9e42241d7cd91d001773b0b616d523dd80e13c6c2cab860b1234ef1b9ffc1526"),
			// 5E1jLYfLdUQKrFrtqoKgFrRvxM3oQPMbf6DfcsrugZZ5Bn8d
			array_bytes::hex2array_unchecked("5633b70b80a6c8bb16270f82cca6d56b27ed7b76c8fd5af2986a25a4788ce440")
				.unchecked_into(),
			// 5DhKqkHRkndJu8vq7pi2Q5S3DfftWJHGxbEUNH43b46qNspH
			array_bytes::hex2array_unchecked("482a3389a6cf42d8ed83888cfd920fec738ea30f97e44699ada7323f08c3380a")
				.unchecked_into(),
			// 5DhKqkHRkndJu8vq7pi2Q5S3DfftWJHGxbEUNH43b46qNspH
			array_bytes::hex2array_unchecked("482a3389a6cf42d8ed83888cfd920fec738ea30f97e44699ada7323f08c3380a")
				.unchecked_into(),
			// 5DhKqkHRkndJu8vq7pi2Q5S3DfftWJHGxbEUNH43b46qNspH
			array_bytes::hex2array_unchecked("482a3389a6cf42d8ed83888cfd920fec738ea30f97e44699ada7323f08c3380a")
				.unchecked_into(),
			// 5DhKqkHRkndJu8vq7pi2Q5S3DfftWJHGxbEUNH43b46qNspH
			array_bytes::hex2array_unchecked("482a3389a6cf42d8ed83888cfd920fec738ea30f97e44699ada7323f08c3380a")
				.unchecked_into(),
			// 5GQx4FToRBPqfani6o7owFJE1UstiviqbPP7HPWyvtXWWukn
			array_bytes::hex2array_unchecked("036a818b3f59579c5fbbe4fede64f49dbf090ba883eb2a175d5ca90e5adb5f0b3e")
				.unchecked_into(),
		),
		(
			// 5HYZnKWe5FVZQ33ZRJK1rG3WaLMztxWrrNDb1JRwaHHVWyP9
			array_bytes::hex_n_into_unchecked("f26cdb14b5aec7b2789fd5ca80f979cef3761897ae1f37ffb3e154cbcc1c2663"),
			// 5EPQdAQ39WQNLCRjWsCk5jErsCitHiY5ZmjfWzzbXDoAoYbn
			array_bytes::hex_n_into_unchecked("66bc1e5d275da50b72b15de072a2468a5ad414919ca9054d2695767cf650012f"),
			// 5DMa31Hd5u1dwoRKgC4uvqyrdK45RHv3CpwvpUC1EzuwDit4
			array_bytes::hex2array_unchecked("3919132b851ef0fd2dae42a7e734fe547af5a6b809006100f48944d7fae8e8ef")
				.unchecked_into(),
			// 5C4vDQxA8LTck2xJEy4Yg1hM9qjDt4LvTQaMo4Y8ne43aU6x
			array_bytes::hex2array_unchecked("00299981a2b92f878baaf5dbeba5c18d4e70f2a1fcd9c61b32ea18daf38f4378")
				.unchecked_into(),
			// 5C4vDQxA8LTck2xJEy4Yg1hM9qjDt4LvTQaMo4Y8ne43aU6x
			array_bytes::hex2array_unchecked("00299981a2b92f878baaf5dbeba5c18d4e70f2a1fcd9c61b32ea18daf38f4378")
				.unchecked_into(),
			// 5C4vDQxA8LTck2xJEy4Yg1hM9qjDt4LvTQaMo4Y8ne43aU6x
			array_bytes::hex2array_unchecked("00299981a2b92f878baaf5dbeba5c18d4e70f2a1fcd9c61b32ea18daf38f4378")
				.unchecked_into(),
			// 5C4vDQxA8LTck2xJEy4Yg1hM9qjDt4LvTQaMo4Y8ne43aU6x
			array_bytes::hex2array_unchecked("00299981a2b92f878baaf5dbeba5c18d4e70f2a1fcd9c61b32ea18daf38f4378")
				.unchecked_into(),
			// 5FCu2pY928VVHPgnNVJssvxFJZECyNe1CyH3WTG79Wisx58B
			array_bytes::hex2array_unchecked("020ce02b963548f9f8ade8765f7a4a06638c17819c78422a1cc35b647873583eef")
				.unchecked_into(),
		),
	];

	// generated with secret: subkey inspect "$secret"/fir
	let root_key: AccountId = array_bytes::hex_n_into_unchecked(
		// 5Ff3iXP75ruzroPWRP2FYBHWnmGGBSb63857BgnzCoXNxfPo
		"9ee5e5bdc0ec239eb164f865ecc345ce4c88e76ee002e0f7e318097347471809",
	);

	let endowed_accounts: Vec<AccountId> = vec![root_key.clone()];
	(initial_authorities, root_key, endowed_accounts)
}

fn staging_testnet_genesis_patch() -> serde_json::Value {
	let (initial_authorities, root_key, endowed_accounts) =
		configure_accounts_for_staging_testnet();
	testnet_genesis_patch(initial_authorities, vec![], root_key, endowed_accounts)
}

/// Staging testnet config.
pub fn staging_testnet_config() -> ChainSpec {
	ChainSpec::builder(wasm_binary_unwrap(), Default::default())
		.with_name("Staging Testnet")
		.with_id("staging_testnet")
		.with_chain_type(ChainType::Live)
		.with_genesis_config_preset_name(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET)
		.with_genesis_config_patch(staging_testnet_genesis_patch())
		.with_telemetry_endpoints(
			TelemetryEndpoints::new(vec![(STAGING_TELEMETRY_URL.to_string(), 0)])
				.expect("Staging telemetry url is valid; qed"),
		)
		.build()
}

/// Configure the accounts for the testnet.
///
/// * Adds `initial_authorities` and `initial_nominators` to endowed accounts if missing.
/// * Sets up the stakers consisting of the `initial_authorities` and `initial_nominators`.
fn configure_accounts(
	initial_authorities: Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		MixnetId,
		BeefyId,
	)>,
	initial_nominators: Vec<AccountId>,
	endowed_accounts: Vec<AccountId>,
	stash: Balance,
) -> (
	Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		MixnetId,
		BeefyId,
	)>,
	Vec<AccountId>,
	Vec<Staker>,
) {
	let mut endowed_accounts = endowed_accounts;
	// endow all authorities and nominators.
	initial_authorities
		.iter()
		.map(|x| &x.0)
		.chain(initial_nominators.iter())
		.for_each(|x| {
			if !endowed_accounts.contains(x) {
				endowed_accounts.push(x.clone())
			}
		});

	// stakers: all validators and nominators.
	let mut rng = rand::thread_rng();
	let stakers = initial_authorities
		.iter()
		.map(|x| (x.0.clone(), x.0.clone(), stash, StakerStatus::Validator))
		.chain(initial_nominators.iter().map(|x| {
			use rand::{seq::SliceRandom, Rng};
			let limit = (MaxNominations::get() as usize).min(initial_authorities.len());
			let count = rng.gen::<usize>() % limit;
			let nominations = initial_authorities
				.as_slice()
				.choose_multiple(&mut rng, count)
				.into_iter()
				.map(|choice| choice.0.clone())
				.collect::<Vec<_>>();
			(x.clone(), x.clone(), stash, StakerStatus::Nominator(nominations))
		}))
		.collect::<Vec<_>>();

	(initial_authorities, endowed_accounts, stakers)
}

/// Helper function to create RuntimeGenesisConfig json patch for testing.
pub fn testnet_genesis_patch(
	initial_authorities: Vec<(
		AccountId,
		AccountId,
		GrandpaId,
		BabeId,
		ImOnlineId,
		AuthorityDiscoveryId,
		MixnetId,
		BeefyId,
	)>,
	initial_nominators: Vec<AccountId>,
	root_key: AccountId,
	endowed_accounts: Vec<AccountId>,
) -> serde_json::Value {
	let (initial_authorities, endowed_accounts, stakers) =
		configure_accounts(initial_authorities, initial_nominators, endowed_accounts, STASH);

	let validator_count = initial_authorities.len();
	let minimum_validator_count = validator_count;

	let collective = collective(&endowed_accounts);

	serde_json::json!({
		"balances": {
			"balances": endowed_accounts.iter().cloned().map(|x| (x, ENDOWMENT)).collect::<Vec<_>>()
		},
		"session": {
			"keys": initial_authorities
			.iter()
			.map(|x| {
				(
					x.0.clone(),
					// stash account is controller
					x.0.clone(),
					session_keys_json(
						x.2.clone(),
						x.3.clone(),
						x.4.clone(),
						x.5.clone(),
						x.6.clone(),
						x.7.clone(),
					)
				)
			})
			.collect::<Vec<_>>()
		},
		"elections": {
			"members": collective.iter().cloned().map(|member| (member, STASH)).collect::<Vec<_>>(),
		},
		"technicalCommittee": {
			"members": collective,
		},
		"staking": {
			"validatorCount": validator_count,
			"minimumValidatorCount": minimum_validator_count,
			"invulnerables": initial_authorities
				.iter()
				.map(|x| x.0.clone())
				.collect::<Vec<_>>(),
			"stakers": stakers,
		},
		"sudo": {
			"key": root_key,
		},
		"revive": {
			"mappedAccounts": if <kitchensink_runtime::Runtime as pallet_revive::Config>::AutoMap::get() {
				vec![]
			} else {
				endowed_accounts.iter().filter(|x| !<kitchensink_runtime::Runtime as pallet_revive::Config>::AddressMapper::is_eth_derived(x)).cloned().collect::<Vec<_>>()
			}
		}
	})
}

/// Creates the session keys as defined by the runtime.
fn session_keys_json(
	grandpa: GrandpaId,
	babe: BabeId,
	im_online: ImOnlineId,
	authority_discovery: AuthorityDiscoveryId,
	mixnet: MixnetId,
	beefy: BeefyId,
) -> serde_json::Value {
	serde_json::json!({
		"authority_discovery": authority_discovery,
		"babe": babe,
		"beefy": beefy,
		"grandpa": grandpa,
		"im_online": im_online,
		"mixnet": mixnet
	})
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

fn props() -> Properties {
	let mut properties = Properties::new();
	properties.insert("tokenDecimals".to_string(), 12.into());
	properties
}

/// Development config (single validator Alice).
pub fn development_config() -> ChainSpec {
	ChainSpec::builder(wasm_binary_unwrap(), Default::default())
		.with_name("Development")
		.with_id("dev")
		.with_chain_type(ChainType::Development)
		.with_properties(props())
		.with_genesis_config_preset_name(sp_genesis_builder::DEV_RUNTIME_PRESET)
		.build()
}

/// Local testnet config (multivalidator Alice + Bob).
pub fn local_testnet_config() -> ChainSpec {
	ChainSpec::builder(wasm_binary_unwrap(), Default::default())
		.with_name("Local Testnet")
		.with_id("local_testnet")
		.with_chain_type(ChainType::Local)
		.with_genesis_config_preset_name(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET)
		.build()
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::service::{new_full_base, NewFullBase};
	use kitchensink_runtime::genesis_config_presets::well_known_including_eth_accounts;
	use sc_service_test;
	use sp_runtime::{AccountId32, BuildStorage};

	/// Local testnet config (single validator - Alice).
	pub fn integration_test_config_with_single_authority() -> ChainSpec {
		ChainSpec::builder(wasm_binary_unwrap(), Default::default())
			.with_name("Integration Test")
			.with_id("test")
			.with_chain_type(ChainType::Development)
			.with_genesis_config_preset_name(sp_genesis_builder::DEV_RUNTIME_PRESET)
			.build()
	}

	/// Local testnet config (multivalidator Alice + Bob).
	pub fn integration_test_config_with_two_authorities() -> ChainSpec {
		ChainSpec::builder(wasm_binary_unwrap(), Default::default())
			.with_name("Integration Test")
			.with_id("test")
			.with_chain_type(ChainType::Local)
			.with_genesis_config_preset_name(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET)
			.build()
	}

	fn eth_account(from: subxt_signer::eth::Keypair) -> AccountId32 {
		let mut account_id = AccountId32::new([0xEE; 32]);
		<AccountId32 as AsMut<[u8; 32]>>::as_mut(&mut account_id)[..20]
			.copy_from_slice(&from.public_key().to_account_id().as_ref());
		account_id
	}

	#[test]
	#[ignore]
	fn test_connectivity() {
		sp_tracing::try_init_simple();

		sc_service_test::connectivity(integration_test_config_with_two_authorities(), |config| {
			let NewFullBase { task_manager, client, network, sync, transaction_pool, .. } =
				new_full_base::<sc_network::NetworkWorker<_, _>>(
					config,
					None,
					false,
					Default::default(),
					|_, _| (),
				)?;
			Ok(sc_service_test::TestNetComponents::new(
				task_manager,
				client,
				network,
				sync,
				transaction_pool,
			))
		});
	}

	#[test]
	fn test_create_development_chain_spec() {
		development_config().build_storage().unwrap();
	}

	#[test]
	fn test_create_local_testnet_chain_spec() {
		local_testnet_config().build_storage().unwrap();
	}

	#[test]
	fn test_staging_test_net_chain_spec() {
		staging_testnet_config().build_storage().unwrap();
	}

	#[test]
	fn ensure_eth_accounts_are_in_endowed() {
		let alith = eth_account(subxt_signer::eth::dev::alith());
		let baltathar = eth_account(subxt_signer::eth::dev::baltathar());

		let endowed = well_known_including_eth_accounts();

		assert!(endowed.contains(&alith), "Alith must be in endowed for integration tests");
		assert!(endowed.contains(&baltathar), "Baltathar must be in endowed for integration tests");
	}
}
