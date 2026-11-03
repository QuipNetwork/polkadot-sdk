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

use crate::{
	ClientError, H160,
	subxt_client::{
		self, SrcChainConfig,
		runtime_types::pallet_revive::storage::{AccountType, ContractInfo},
	},
};
use subxt::client::OnlineClientAtBlock;

/// A wrapper around the Substrate Storage API for a given block.
#[derive(Clone)]
pub struct StorageApi {
	at_block: OnlineClientAtBlock<SrcChainConfig>,
}

impl StorageApi {
	/// Create a new instance of the StorageApi anchored at `at_block`.
	pub fn new(at_block: OnlineClientAtBlock<SrcChainConfig>) -> Self {
		Self { at_block }
	}

	/// Get the contract info for the given contract address.
	pub async fn get_contract_info(
		&self,
		contract_address: &H160,
	) -> Result<ContractInfo, ClientError> {
		let contract_address: subxt::utils::H160 = contract_address.0.into();

		let query = subxt_client::storage().revive().account_info_of().unvalidated();
		let entry = self.at_block.storage().entry(query)?;
		let Some(info) = entry.try_fetch((contract_address,)).await? else {
			return Err(ClientError::ContractNotFound);
		};
		let info = info.decode()?;

		let AccountType::Contract(contract_info) = info.account_type else {
			return Err(ClientError::ContractNotFound);
		};

		Ok(contract_info)
	}

	/// Get the contract trie id for the given contract address.
	pub async fn get_contract_trie_id(&self, address: &H160) -> Result<Vec<u8>, ClientError> {
		let ContractInfo { trie_id, .. } = self.get_contract_info(address).await?;
		Ok(trie_id.0)
	}
}
