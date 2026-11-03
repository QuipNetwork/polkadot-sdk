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
use pallet_revive_eth_rpc::{EthRpcClient, ReceiptInfo, example::TransactionBuilder};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let client = Arc::new(HttpClientBuilder::default().build("http://localhost:8545")?);

	let alith = Account::default();
	let alith_address = alith.address();
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let value = 1_000_000_000_000_000_000_000u128.into();

	let print_balance = || async {
		let balance = client.get_balance(alith_address, Default::default()).await?;
		println!("Alith     {alith_address:?} balance: {balance:?}");
		let balance = client.get_balance(ethan.address(), Default::default()).await?;
		println!("ethan {:?} balance: {balance:?}", ethan.address());
		anyhow::Result::<()>::Ok(())
	};

	print_balance().await?;
	println!("\n\n=== Transferring  ===\n\n");

	let tx = TransactionBuilder::new(client.clone())
		.signer(alith)
		.value(value)
		.to(ethan.address())
		.send()
		.await?;
	println!("Transaction hash: {:?}", tx.hash());

	let ReceiptInfo { block_number, gas_used, status, .. } = tx.wait_for_receipt().await?;
	println!("Receipt: ");
	println!("- Block number: {block_number}");
	println!("- Gas used: {gas_used}");
	println!("- Success: {status:?}");

	print_balance().await?;
	Ok(())
}
