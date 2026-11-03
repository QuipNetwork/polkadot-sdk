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
use jsonrpsee::http_client::HttpClientBuilder;
use pallet_revive::evm::Account;
use pallet_revive_eth_rpc::{BlockNumberOrTag, EthRpcClient};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let account = Account::default();
	println!("Account address: {:?}", account.address());

	let client = Arc::new(HttpClientBuilder::default().build("http://localhost:8545")?);

	let block = client.get_block_by_number(BlockNumberOrTag::Latest, false).await?;
	println!("Latest block: {block:#?}");

	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	println!("Account nonce: {nonce:?}");

	let balance = client.get_balance(account.address(), Default::default()).await?;
	println!("Account balance: {balance:?}");

	let sync_state = client.syncing().await?;
	println!("Sync state: {sync_state:?}");

	Ok(())
}
