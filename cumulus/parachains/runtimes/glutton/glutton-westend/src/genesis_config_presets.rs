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

//! # Glutton Westend Runtime genesis config presets

use crate::*;
use alloc::vec::Vec;
use cumulus_primitives_core::ParaId;
use frame_support::build_struct_json_patch;
use parachains_common::AuraId;
use sp_genesis_builder::PresetId;
use sp_keyring::Sr25519Keyring;

/// Default para-id baked into the presets.
///
/// The preset API cannot take a dynamic para-id, so the presets are built with this fixed value.
/// Callers that need a specific para-id (e.g. the `glutton-westend-*-<id>` CLI chain specs) patch
/// `parachain_info.parachain_id` over the preset output instead of relying on this default.
pub const DEFAULT_GLUTTON_PARA_ID: ParaId = ParaId::new(1300);

pub fn glutton_westend_genesis(
	authorities: Vec<AuraId>,
	sudo: Option<AccountId>,
	id: ParaId,
) -> serde_json::Value {
	build_struct_json_patch!(RuntimeGenesisConfig {
		parachain_info: ParachainInfoConfig { parachain_id: id },
		aura: AuraConfig { authorities },
		sudo: SudoConfig { key: sudo }
	})
}

/// Provides the JSON representation of predefined genesis config for given `id`.
pub fn get_preset(id: &PresetId) -> Option<Vec<u8>> {
	let patch = match id.as_ref() {
		sp_genesis_builder::LOCAL_TESTNET_RUNTIME_PRESET => glutton_westend_genesis(
			// initial collators.
			vec![Sr25519Keyring::Alice.public().into(), Sr25519Keyring::Bob.public().into()],
			Some(Sr25519Keyring::Alice.to_account_id()),
			DEFAULT_GLUTTON_PARA_ID,
		),
		sp_genesis_builder::DEV_RUNTIME_PRESET => glutton_westend_genesis(
			// initial collators.
			vec![Sr25519Keyring::Alice.public().into()],
			Some(Sr25519Keyring::Alice.to_account_id()),
			DEFAULT_GLUTTON_PARA_ID,
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
