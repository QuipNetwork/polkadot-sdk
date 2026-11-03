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

use cumulus_primitives_core::ParaId;
use hex_literal::hex;
use parachains_common::AccountId;
use polkadot_omni_node_lib::chain_spec::{Extensions, GenericChainSpec};
use sc_service::ChainType;

pub fn get_penpal_chain_spec(id: ParaId, relay_chain: &str) -> GenericChainSpec {
	// Give your base currency a unit name and decimal places
	let mut properties = sc_chain_spec::Properties::new();
	properties.insert("tokenSymbol".into(), "UNIT".into());
	properties.insert("tokenDecimals".into(), 12u32.into());
	properties.insert("ss58Format".into(), 42u32.into());

	GenericChainSpec::builder(
		penpal_runtime::WASM_BINARY.expect("WASM binary was not built, please build it!"),
		Extensions::new_with_relay_chain(relay_chain.into()),
	)
	.with_name("Penpal Parachain")
	.with_id(&format!("penpal-{}", relay_chain.replace("-local", "")))
	.with_chain_type(ChainType::Local)
	.with_genesis_config_preset_name(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET)
	.with_genesis_config_patch(serde_json::json!({
		"parachainInfo": {
			"parachainId": id,
		},
	}))
	.build()
}

pub fn staging_penpal_local_config() -> GenericChainSpec {
	GenericChainSpec::builder(
		penpal_runtime::WASM_BINARY.expect("WASM binary was not built, please build it!"),
		Extensions::new_with_relay_chain("rococo-local".into()),
	)
	.with_name("Staging Rococo Penpal Local")
	.with_id("staging_testnet")
	.with_chain_type(ChainType::Live)
	.with_genesis_config_preset_name(sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET)
	.with_genesis_config_patch(testnet_genesis_patch(
		hex!["d43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d"].into(),
		vec![hex!["d43593c715fdd31c61141abd04a99fd6822c8558854ccde39a5684e7a56da27d"].into()],
		1000.into(),
	))
	.build()
}

pub(crate) fn testnet_genesis_patch(
	root_key: AccountId,
	endowed_accounts: Vec<AccountId>,
	id: ParaId,
) -> serde_json::Value {
	serde_json::json!({
			"balances": {
					"balances": endowed_accounts.iter().cloned().map(|k| (k, 1u64 << 60)).collect::<Vec<_>>(),
			},
			"sudo": { "key": Some(root_key) },
			"parachainInfo": {
					"parachainId": id,
			},
	})
}

#[cfg(test)]
mod test {
	use super::*;
	use penpal_runtime::BuildStorage;

	#[test]
	fn staging_penpal_local_config_works() {
		let chain_spec = Box::new(staging_penpal_local_config());
		chain_spec
			.build_storage()
			.expect("build_storage from staging chain-spec (default) config should works.");
	}

	#[test]
	fn penpal_chain_spec_works() {
		let chain_spec = Box::new(get_penpal_chain_spec(1002.into(), "rococo"));
		chain_spec
			.build_storage()
			.expect("build_storage from staging chain-spec (default) config should works.");
	}

	#[test]
	fn staging_penpal_invalid_config_err() {
		use parachains_common::AuraId;
		use serde_json::{json, Value};
		use sp_core::crypto::UncheckedInto;

		let aura_auth: AuraId =
			hex!["aad9fa2249f87a210a0f93400b7f90e47b810c6d65caa0ca3f5af982904c2a33"]
				.unchecked_into();

		let chain_spec = Box::new(staging_penpal_local_config());
		let mut chain_spec_json: Value = serde_json::from_str(
			&chain_spec
				.as_json(false)
				.expect("serialization to json is expected to work. qed."),
		)
		.expect("serialization to json Value is expected to work. qed.");
		chain_spec_json["genesis"]["runtimeGenesis"]["patch"]["aura"] =
			json!({"authorities" : vec![ aura_auth ] });

		let chain_spec_invalid_config =
			GenericChainSpec::from_json_bytes(chain_spec_json.to_string().as_bytes().to_vec())
				.expect("parse json content into a ChainSpec should works. qed");
		let result = chain_spec_invalid_config.build_storage();
		assert!(result.is_err());
	}
}
