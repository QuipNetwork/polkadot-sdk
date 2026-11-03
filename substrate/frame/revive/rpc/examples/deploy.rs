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
use pallet_revive::{
	create1,
	evm::{Account, U256},
};
use pallet_revive_eth_rpc::{EthRpcClient, ReceiptInfo, example::TransactionBuilder};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	env_logger::init();
	let account = Account::default();

	let data = vec![];
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let input = bytes.into_iter().chain(data.clone()).collect::<Vec<u8>>();

	println!("Account:");
	println!("- address: {:?}", account.address());
	println!("- substrate: {}", account.substrate_account());
	let client = Arc::new(HttpClientBuilder::default().build("http://localhost:8545")?);

	println!("\n\n=== Deploying contract ===\n\n");

	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone())
		.value(5_000_000_000_000u128.into())
		.input(input)
		.send()
		.await?;

	println!("Deploy Tx hash: {:?}", tx.hash());
	let ReceiptInfo { block_number, gas_used, contract_address, .. } =
		tx.wait_for_receipt().await?;

	let contract_address = contract_address.unwrap();
	assert_eq!(contract_address, create1(&account.address(), nonce.try_into().unwrap()));

	println!("Receipt:");
	println!("- Block number:     {block_number}");
	println!("- Gas estimated:    {:?}", tx.gas());
	println!("- Gas used:         {gas_used}");
	println!("- Contract address: {contract_address:?}");
	let balance = client.get_balance(contract_address, Default::default()).await?;
	println!("- Contract balance: {balance:?}");

	if std::env::var("SKIP_CALL").is_ok() {
		return Ok(());
	}

	println!("\n\n=== Calling contract ===\n\n");
	let tx = TransactionBuilder::new(client.clone())
		.value(U256::from(1_000_000u32))
		.to(contract_address)
		.send()
		.await?;

	println!("Contract call tx hash: {:?}", tx.hash());
	let ReceiptInfo { block_number, gas_used, to, .. } = tx.wait_for_receipt().await?;
	println!("Receipt:");
	println!("- Block number:  {block_number}");
	println!("- Gas used:      {gas_used}");
	println!("- Gas estimated: {:?}", tx.gas());
	println!("- To:            {to:?}");
	Ok(())
}
