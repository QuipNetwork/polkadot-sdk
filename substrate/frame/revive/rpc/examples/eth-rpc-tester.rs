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
use clap::Parser;
use jsonrpsee::http_client::HttpClientBuilder;
use pallet_revive::evm::Account;
use pallet_revive_eth_rpc::{EthRpcClient, ReceiptInfo, example::TransactionBuilder};
use std::sync::Arc;
use tokio::{
	io::{AsyncBufReadExt, BufReader},
	process::{Child, ChildStderr, Command},
	signal::unix::{SignalKind, signal},
};

const DOCKER_CONTAINER_NAME: &str = "eth-rpc-test";

#[derive(Parser, Debug)]
#[clap(author, about, version)]
pub struct CliCommand {
	/// The eth-rpc url to connect to
	#[clap(long, default_value = "http://127.0.0.1:8545")]
	pub rpc_url: String,

	/// The parity docker image e.g eth-rpc:master-fb2e414f
	/// When not specified, no eth-rpc docker image is started
	/// and the test runs against the provided `rpc_url` directly.
	#[clap(long)]
	docker_image: Option<String>,

	/// The docker binary
	/// Either docker or podman
	#[clap(long, default_value = "docker")]
	docker_bin: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let CliCommand { docker_bin, rpc_url, docker_image, .. } = CliCommand::parse();

	let Some(docker_image) = docker_image else {
		println!("Docker image not specified, using: {rpc_url:?}");
		return test_eth_rpc(&rpc_url).await;
	};

	let mut docker_process = start_docker(&docker_bin, &docker_image)?;
	let stderr = docker_process.stderr.take().unwrap();

	tokio::select! {
		result = docker_process.wait() => {
			println!("docker failed: {result:?}");
		}
		_ = interrupt() => {
			kill_docker().await?;
		}
		_ = wait_and_test_eth_rpc(stderr, &rpc_url) => {
			kill_docker().await?;
		}
	}

	Ok(())
}

async fn interrupt() {
	let mut sigint = signal(SignalKind::interrupt()).expect("failed to listen for SIGINT");
	let mut sigterm = signal(SignalKind::terminate()).expect("failed to listen for SIGTERM");

	tokio::select! {
		_ = sigint.recv() => {},
		_ = sigterm.recv() => {},
	}
}

fn start_docker(docker_bin: &str, docker_image: &str) -> anyhow::Result<Child> {
	let docker_process = Command::new(docker_bin)
		.args([
			"run",
			"--name",
			DOCKER_CONTAINER_NAME,
			"--rm",
			"-p",
			"8545:8545",
			&format!("docker.io/paritypr/{docker_image}"),
			"--node-rpc-url",
			"wss://westend-asset-hub-rpc.polkadot.io",
			"--rpc-cors",
			"all",
			"--unsafe-rpc-external",
			"--log=sc_rpc_server:info",
		])
		.stderr(std::process::Stdio::piped())
		.kill_on_drop(true)
		.spawn()?;

	Ok(docker_process)
}

async fn kill_docker() -> anyhow::Result<()> {
	Command::new("docker").args(["kill", DOCKER_CONTAINER_NAME]).output().await?;
	Ok(())
}

async fn wait_and_test_eth_rpc(stderr: ChildStderr, rpc_url: &str) -> anyhow::Result<()> {
	let mut reader = BufReader::new(stderr).lines();
	while let Some(line) = reader.next_line().await? {
		println!("{line}");
		if line.contains("Running JSON-RPC server") {
			break;
		}
	}

	test_eth_rpc(rpc_url).await
}

async fn test_eth_rpc(rpc_url: &str) -> anyhow::Result<()> {
	let account = Account::default();
	let data = vec![];
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let input = bytes.into_iter().chain(data).collect::<Vec<u8>>();

	println!("Account:");
	println!("- address: {:?}", account.address());
	println!("- substrate address: {}", account.substrate_account());
	let client = Arc::new(HttpClientBuilder::default().build(rpc_url)?);

	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let balance = client.get_balance(account.address(), Default::default()).await?;
	println!("-  nonce: {nonce:?}");
	println!("-  balance: {balance:?}");

	println!("\n\n=== Deploying dummy contract ===\n\n");
	let tx = TransactionBuilder::new(client.clone()).input(input).send().await?;

	println!("Hash: {:?}", tx.hash());
	println!("Waiting for receipt...");
	let ReceiptInfo { block_number, gas_used, contract_address, .. } =
		tx.wait_for_receipt().await?;

	let contract_address = contract_address.unwrap();
	println!("\nReceipt:");
	println!("Block explorer: https://westend-asset-hub-eth-explorer.parity.io/{:?}", tx.hash());
	println!("- Block number: {block_number}");
	println!("- Gas used:     {gas_used}");
	println!("- Address:      {contract_address:?}");

	println!("\n\n=== Calling dummy contract ===\n\n");
	let tx = TransactionBuilder::new(client.clone()).to(contract_address).send().await?;

	println!("Hash: {:?}", tx.hash());
	println!("Waiting for receipt...");

	let ReceiptInfo { block_number, gas_used, to, .. } = tx.wait_for_receipt().await?;
	println!("\nReceipt:");
	println!("Block explorer: https://westend-asset-hub-eth-explorer.parity.io/{:?}", tx.hash());
	println!("- Block number: {block_number}");
	println!("- Gas used:     {gas_used}");
	println!("- To:           {to:?}");
	Ok(())
}
