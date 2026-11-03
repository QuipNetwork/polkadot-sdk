// This file is part of Substrate.

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

use crate::extrinsic::ExtrinsicBuilder;
use codec::{Decode, Encode};
use sc_client_api::UsageProvider;
use sp_api::{ApiExt, Core, Metadata, ProvideRuntimeApi};
use sp_runtime::{traits::Block as BlockT, OpaqueExtrinsic};
use std::sync::Arc;
use subxt::{
	client::{OfflineClient, OfflineClientAtBlock},
	config::substrate::{SpecVersionForRange, SubstrateExtrinsicParamsBuilder},
	utils::H256,
	SubstrateConfig,
};

/// Spec and transaction version information required to construct extrinsics offline.
#[derive(Clone, Copy, Debug)]
pub struct RuntimeVersion {
	/// The runtime spec version.
	pub spec_version: u32,
	/// The runtime transaction version.
	pub transaction_version: u32,
}

pub type SubstrateRemarkBuilder = DynamicRemarkBuilder;

/// Remark builder that can be used to build simple extrinsics for FRAME-based runtimes
/// configured with [`SubstrateConfig`].
pub struct DynamicRemarkBuilder {
	offline_client_at_block: OfflineClientAtBlock<SubstrateConfig>,
}

impl DynamicRemarkBuilder {
	/// Initializes a new remark builder from a client.
	///
	/// This will first fetch metadata and runtime version from the runtime and then
	/// construct an offline client that provides the extrinsics.
	pub fn new_from_client<Client, Block>(client: Arc<Client>) -> sc_cli::Result<Self>
	where
		Block: BlockT,
		Client: UsageProvider<Block> + ProvideRuntimeApi<Block>,
		Client::Api: Metadata<Block> + Core<Block>,
	{
		let genesis = client.usage_info().chain.best_hash;
		let api = client.runtime_api();

		let Ok(Some(metadata_api_version)) = api.api_version::<dyn Metadata<Block>>(genesis) else {
			return Err("Unable to fetch metadata runtime API version.".to_string().into());
		};

		log::debug!("Found metadata API version {}.", metadata_api_version);
		let opaque_metadata = if metadata_api_version > 1 {
			let Ok(supported_metadata_versions) = api.metadata_versions(genesis) else {
				return Err("Unable to fetch metadata versions".to_string().into());
			};

			let latest = supported_metadata_versions
				.into_iter()
				.max()
				.ok_or("No stable metadata versions supported".to_string())?;

			api.metadata_at_version(genesis, latest)
				.map_err(|e| format!("Unable to fetch metadata: {:?}", e))?
				.ok_or("Unable to decode metadata".to_string())?
		} else {
			// Fall back to using the non-versioned metadata API.
			api.metadata(genesis)
				.map_err(|e| format!("Unable to fetch metadata: {:?}", e))?
		};

		let version = api.version(genesis).unwrap();
		let runtime_version = RuntimeVersion {
			spec_version: version.spec_version,
			transaction_version: version.transaction_version,
		};
		let metadata = subxt::Metadata::decode(&mut (*opaque_metadata).as_slice())?;
		let genesis_bytes: [u8; 32] =
			genesis.encode().as_slice().try_into().map_err(|_| {
				"Incompatible hash types: expected 32-byte genesis hash".to_string()
			})?;
		let genesis_hash = H256::from(genesis_bytes);

		Ok(Self::new(metadata, genesis_hash, runtime_version))
	}

	/// Constructs a new remark builder for a [`SubstrateConfig`] chain.
	pub fn new(
		metadata: subxt::Metadata,
		genesis_hash: H256,
		runtime_version: RuntimeVersion,
	) -> Self {
		let config = SubstrateConfig::builder()
			.set_genesis_hash(genesis_hash)
			.set_metadata_for_spec_versions(std::iter::once((
				runtime_version.spec_version,
				metadata.into(),
			)))
			.set_spec_version_for_block_ranges(std::iter::once(SpecVersionForRange {
				block_range: 0..u64::MAX,
				spec_version: runtime_version.spec_version,
				transaction_version: runtime_version.transaction_version,
			}))
			.build();
		let offline_client = OfflineClient::<SubstrateConfig>::new_with_config(config);
		// The block number here is only used to look up the spec version. Any number in the
		// configured range works; we use 0 since the range is `0..u64::MAX`.
		let offline_client_at_block = offline_client
			.at_block(0u64)
			.expect("range was configured to span all block numbers; qed");
		Self { offline_client_at_block }
	}
}

impl ExtrinsicBuilder for DynamicRemarkBuilder {
	fn pallet(&self) -> &str {
		"system"
	}

	fn extrinsic(&self) -> &str {
		"remark"
	}

	fn build(&self, nonce: u32) -> std::result::Result<OpaqueExtrinsic, &'static str> {
		let signer = subxt_signer::sr25519::dev::alice();
		let dynamic_tx = subxt::dynamic::tx("System", "remark", vec![Vec::<u8>::new()]);

		let params = SubstrateExtrinsicParamsBuilder::<SubstrateConfig>::new()
			.nonce(nonce.into())
			.build();

		// Default transaction parameters assume a nonce of 0.
		let transaction = self
			.offline_client_at_block
			.tx()
			.create_signable_offline(&dynamic_tx, params)
			.map_err(|_| "Unable to create signable transaction")?
			.sign(&signer)
			.map_err(|_| "Unable to sign transaction")?;
		let mut encoded = transaction.into_encoded();

		OpaqueExtrinsic::try_from_encoded_extrinsic(&mut encoded)
			.map_err(|_| "Unable to construct OpaqueExtrinsic")
	}
}
