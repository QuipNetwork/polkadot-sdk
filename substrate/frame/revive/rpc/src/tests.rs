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
//! Test the eth-rpc cli with the kitchensink node.
//! This only includes basic transaction tests, most of the other tests are in the
//! [evm-test-suite](https://github.com/paritytech/evm-test-suite) repository.

use crate::{
	BlockHeader, BlockInfoProvider, BoundedOneOrMany, ChainMetadata, DbContext, DebugRpcClient,
	EthRpcClient, FilterResults, Log, ReceiptExtractor, ReceiptProvider, SubscriptionItem,
	SubscriptionKind, SubscriptionOptions, SubxtBlockInfoProvider, SyncLabel,
	cli::{self, CliCommand},
	client::{
		Client, GapFillRequest, SubscriptionGapQueue, connect,
		version_aware_runtime_api::VersionAwareRuntimeApiProvider,
	},
	example::TransactionBuilder,
	subxt_client::{self, SrcChainConfig},
};
use alloy_network::EthereumWallet;
use alloy_primitives::{Address as AlloyAddress, B256, Bytes as AlloyBytes, U256 as AlloyU256};
use alloy_provider::{Provider, ProviderBuilder, WsConnect, ext::DebugApi as _};
use alloy_rpc_types::{
	BlockId, BlockNumberOrTag, Filter, TransactionRequest,
	state::{AccountOverride, StateOverride},
};
use alloy_signer_local::PrivateKeySigner;
use anyhow::anyhow;
use clap::Parser;
use jsonrpsee::{
	core::ClientError,
	ws_client::{WsClient, WsClientBuilder},
};
use pallet_revive::{
	create1,
	evm::{Account, H256, TransactionUnsigned, U256},
	precompiles::alloy::{
		self,
		sol_types::{SolCall, SolConstructor, SolEvent, SolInterface},
	},
};
use pallet_revive_fixtures::{Callee, Counter, TwoSlots};
use pallet_revive_types::runtime_api::{
	BlockV1, CallTracerConfigV1, CodeV1, GenericTransactionV1, HashesOrTransactionInfosV1,
	TraceBlockInputPayloadV1, TraceBlockInputPayloadV2, TraceBlockVersionedInputPayload,
	TraceBlockVersionedOutputPayload, TraceEntryV1, TraceV1, TraceV2, TracerTypeV1,
};
use sp_runtime::BoundedVec;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::{sync::Arc, thread};
use subxt::{
	OnlineClient,
	client::OnlineClientAtBlockImpl,
	rpcs::{RpcClient, rpc_params},
	tx::{SubmittableTransaction, TransactionStatus as TxStatus},
};

type NodeSubmittableTransaction =
	SubmittableTransaction<SrcChainConfig, OnlineClientAtBlockImpl<SrcChainConfig>>;
use subxt_signer::eth::Keypair;
use tokio::sync::mpsc;

const LOG_TARGET: &str = "eth-rpc-tests";

/// Create a websocket client with a 120s timeout.
async fn ws_client_with_retry(url: &str) -> WsClient {
	let timeout = tokio::time::Duration::from_secs(120);
	tokio::time::timeout(timeout, async {
		loop {
			if let Ok(client) = WsClientBuilder::default().build(url).await {
				return client;
			} else {
				tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
			}
		}
	})
	.await
	.expect("Hit timeout")
}

struct SharedResources {
	_node_handle: std::thread::JoinHandle<()>,
	_rpc_handle: std::thread::JoinHandle<()>,
}

impl SharedResources {
	fn start() -> Self {
		// Start revive-dev-node
		let _node_handle = thread::spawn(move || {
			if let Err(e) = revive_dev_node::command::run_with_args(vec![
				"--dev".to_string(),
				"--rpc-port=45789".to_string(),
				"-lerror,sc_rpc_server=info,runtime::revive=debug".to_string(),
			]) {
				panic!("Node exited with error: {e:?}");
			}
		});

		// Start the rpc server.
		let args = CliCommand::parse_from([
			"--dev",
			"--rpc-port=45788",
			"--node-rpc-url=ws://localhost:45789",
			"--no-prometheus",
			"-linfo,eth-rpc=debug",
			"--eth-pruning=256",
		]);

		let _rpc_handle = thread::spawn(move || {
			if let Err(e) = cli::run(args) {
				panic!("eth-rpc exited with error: {e:?}");
			}
		});

		Self { _node_handle, _rpc_handle }
	}

	async fn client() -> WsClient {
		ws_client_with_retry("ws://localhost:45788").await
	}

	async fn node_client() -> OnlineClient<SrcChainConfig> {
		OnlineClient::<SrcChainConfig>::from_url(Self::node_rpc_url()).await.unwrap()
	}

	fn node_rpc_url() -> &'static str {
		"ws://localhost:45789"
	}

	/// Creates an alloy HTTP provider connected to the test eth-rpc server, configured with the
	/// default dev account (alith) wallet for signing transactions.
	///
	/// Using alloy's provider in tests ensures that our JSON-RPC types (especially state
	/// overrides, transaction requests, and call responses) are wire-compatible with the models
	/// used by the wider Ethereum ecosystem. If alloy can successfully serialize a request and
	/// deserialize our response, external tooling built on alloy will work with our RPC.
	fn provider() -> impl Provider {
		let secret_key = subxt_signer::eth::dev::alith().secret_key();
		let signer = PrivateKeySigner::from_bytes(&secret_key.into()).expect("valid dev key");
		let wallet = EthereumWallet::from(signer);
		ProviderBuilder::new()
			.wallet(wallet)
			.connect_http("http://localhost:45788".parse().unwrap())
	}
}

macro_rules! unwrap_call_err(
	($err:expr) => {
		match $err.downcast_ref::<jsonrpsee::core::client::Error>().unwrap() {
			jsonrpsee::core::client::Error::Call(call) => call,
			_ => panic!("Expected Call error"),
		}
	}
);

// Helper functions
/// Prepare multiple EVM transfer transactions with nonce in descending order
async fn prepare_evm_transactions<Client: EthRpcClient + Sync + Send>(
	client: Arc<Client>,
	signer: Account,
	recipient: pallet_revive::evm::Address,
	amount: U256,
	count: usize,
) -> anyhow::Result<Vec<TransactionBuilder<Client>>> {
	let start_nonce = client.get_transaction_count(signer.address(), Default::default()).await?;

	let mut transactions = Vec::new();
	for i in (0..count).rev() {
		let nonce = start_nonce.saturating_add(U256::from(i as u64));
		let tx_builder = TransactionBuilder::new(Arc::clone(&client))
			.signer(signer.clone())
			.nonce(nonce)
			.value(amount)
			.to(recipient);

		transactions.push(tx_builder);
		log::trace!(target: LOG_TARGET, "Prepared EVM transaction {}/{count} with nonce: {nonce:?}", i + 1);
	}

	Ok(transactions)
}

/// Prepare multiple Substrate transfer transactions with sequential nonces
async fn prepare_substrate_transactions(
	node_client: &OnlineClient<SrcChainConfig>,
	signer: &subxt_signer::sr25519::Keypair,
	count: usize,
) -> anyhow::Result<Vec<NodeSubmittableTransaction>> {
	let mut tx_client = node_client.tx().await?;
	let mut nonce = tx_client.account_nonce(&signer.public_key().into()).await?;
	let mut substrate_txs = Vec::new();
	for i in 0..count {
		let remark_data = format!("Hello from test {}", i);
		let call = subxt::dynamic::tx(
			"System",
			"remark",
			vec![subxt::dynamic::Value::from_bytes(remark_data.as_bytes())],
		);

		let params = subxt::config::polkadot::PolkadotExtrinsicParamsBuilder::new()
			.nonce(nonce)
			.build();

		let tx = tx_client.create_signed(&call, signer, params).await?;
		substrate_txs.push(tx);
		log::trace!(target: LOG_TARGET, "Prepared substrate transaction {i}/{count} with nonce: {nonce}");
		nonce += 1;
	}
	Ok(substrate_txs)
}

/// Submit multiple transactions and return them without waiting for receipts
async fn submit_evm_transactions<Client: EthRpcClient + Sync + Send>(
	transactions: Vec<TransactionBuilder<Client>>,
) -> anyhow::Result<
	Vec<(
		H256,
		pallet_revive::evm::GenericTransaction,
		crate::example::SubmittedTransaction<Client>,
	)>,
> {
	let mut submitted_txs = Vec::new();

	for tx_builder in transactions {
		let tx = tx_builder.send().await?;
		let hash = tx.hash();
		let generic_tx = tx.generic_transaction();
		submitted_txs.push((hash, generic_tx, tx));
	}

	Ok(submitted_txs)
}

/// Submit substrate transactions and return futures for waiting
async fn submit_substrate_transactions(
	substrate_txs: Vec<NodeSubmittableTransaction>,
) -> Vec<impl std::future::Future<Output = Result<(), anyhow::Error>>> {
	let mut futures = Vec::new();

	for (i, tx) in substrate_txs.into_iter().enumerate() {
		let fut = async move {
			match tx.submit_and_watch().await {
				Ok(mut progress) => {
					log::trace!(target: LOG_TARGET, "Substrate tx {i} submitted");
					while let Some(status) = progress.next().await {
						match status {
							Ok(TxStatus::InFinalizedBlock(block)) |
							Ok(TxStatus::InBestBlock(block)) => {
								log::trace!(target: LOG_TARGET,
									"Substrate tx {i} included in block {:?}",
									block.block_hash()
								);
								return Ok(());
							},
							Err(e) => return Err(anyhow::anyhow!("Substrate tx {i} error: {e}")),
							Ok(status) => {
								log::trace!(target: LOG_TARGET, "Substrate tx {i} status {:?}", status);
							},
						}
					}
					Err(anyhow::anyhow!(
						"Failed to get status of submitted substrate tx {i}, assuming error"
					))
				},
				Err(e) => Err(anyhow::anyhow!("Failed to submit substrate tx {i}: {e}")),
			}
		};
		futures.push(fut);
	}

	futures
}

/// Verify all given transaction hashes are in the specified block and accessible via RPC
async fn verify_transactions_in_single_block(
	client: &Arc<WsClient>,
	block_number: U256,
	expected_tx_hashes: &[H256],
) -> anyhow::Result<()> {
	// Fetch the block
	let block = client
		.get_block_by_number(BlockNumberOrTag::Number(block_number.as_u64()), false)
		.await?
		.ok_or_else(|| anyhow!("Block {block_number} should exist"))?;

	let block_tx_hashes = match &block.transactions {
		HashesOrTransactionInfosV1::Hashes(hashes) => hashes.clone(),
		HashesOrTransactionInfosV1::TransactionInfos(infos) => {
			infos.iter().map(|info| info.hash).collect()
		},
	};

	if let Some(missing_hash) =
		expected_tx_hashes.iter().find(|hash| !block_tx_hashes.contains(hash))
	{
		return Err(anyhow!("Transaction {missing_hash:?} not found in block {block_number}"));
	}

	Ok(())
}

/// Wait for the eth-rpc's finalized block to catch up to its best block.
async fn wait_for_finalized_to_reach_best<C: EthRpcClient + Sync>(
	client: &C,
) -> anyhow::Result<()> {
	tokio::time::timeout(tokio::time::Duration::from_secs(5), async {
		let latest = client
			.get_block_by_number(BlockNumberOrTag::Latest, false)
			.await?
			.expect("latest block should exist")
			.number;
		loop {
			let finalized = client.get_block_by_number(BlockNumberOrTag::Finalized, false).await?;
			if finalized.map(|block| block.number == latest).unwrap_or(false) {
				break;
			}
			tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
		}
		anyhow::Ok(())
	})
	.await
	.map_err(|_| anyhow::anyhow!("timed out waiting for finalized block to match best block"))?
}

#[tokio::test]
async fn run_all_eth_rpc_tests() -> anyhow::Result<()> {
	let timeout_duration = tokio::time::Duration::from_secs(300);
	let result = tokio::time::timeout(timeout_duration, run_all_eth_rpc_tests_inner()).await;

	match result {
		Ok(inner_result) => inner_result,
		Err(_) => {
			log::error!(target: LOG_TARGET, "Test timed out after {}s!", timeout_duration.as_secs());
			std::process::exit(1);
		},
	}
}

async fn run_all_eth_rpc_tests_inner() -> anyhow::Result<()> {
	// start node and rpc server
	let _shared = SharedResources::start();
	// Wait for servers to be ready
	let _ = SharedResources::client().await;

	macro_rules! run_tests {
		($($test:ident),+ $(,)?) => {
			$(
				{
					let test_name = stringify!($test);
					log::debug!(target: LOG_TARGET, "Running test: {}", test_name);
					match $test().await {
						Ok(()) => log::debug!(target: LOG_TARGET, "Test passed: {}", test_name),
						Err(err) => panic!("Test {} failed: {err:?}", test_name),
					}
				}
			)+
		};
	}

	run_tests!(
		test_fibonacci_call_via_runtime_api,
		test_trace_block_returns_v1_trace_on_v1_input_and_v2_trace_on_v2_input,
		test_transfer,
		test_deploy_and_call,
		test_receipt_mixed_revert_and_logs_same_block,
		test_runtime_api_dry_run_addr_works,
		test_invalid_transaction,
		test_evm_blocks_should_match,
		test_evm_blocks_hydrated_should_match,
		test_get_block_receipts,
		test_block_hash_for_tag_with_proper_ethereum_block_hash_works,
		test_block_hash_for_tag_with_invalid_ethereum_block_hash_fails,
		test_block_hash_for_tag_with_block_number_works,
		test_block_hash_for_tag_with_block_tags_works,
		test_earliest_block_tag,
		test_get_logs_with_block_tags_works,
		test_multiple_transactions_in_block,
		test_mixed_evm_substrate_transactions,
		test_runtime_pallets_address_upload_code,
		test_new_heads_subscription_delivers_matching_header_via_alloy,
		test_subscribe_new_heads_multiple_blocks,
		test_subscribe_logs,
		test_subscribe_logs_with_address_filter,
		test_subscribe_logs_with_topic_filter,
		test_subscribe_logs_address_filter_excludes_non_matching,
		test_subscribe_logs_with_multiple_addresses_filter,
		test_subscribe_logs_no_event_transaction_ignored,
		test_subscribe_with_invalid_params_rejected,
		test_estimate_gas_of_contract_with_consume_all_gas,
		test_gas_estimation_for_contract_requiring_binary_search,
		test_gas_estimation_with_no_funds_no_gas_specified,
		test_gas_estimation_with_no_funds_and_with_gas_specified,
		test_block_sync_fresh,
		test_block_sync_resume_interrupted,
		test_block_sync_detects_corruption,
		test_block_sync_picks_up_new_blocks,
		test_state_override_balance,
		test_state_override_code_empty_to_evm,
		test_state_override_code_empty_to_pvm,
		test_state_override_code_eoa_to_evm,
		test_state_override_code_eoa_to_pvm,
		test_state_override_code_evm_to_evm,
		test_state_override_code_evm_to_pvm,
		test_state_override_code_pvm_to_evm,
		test_state_override_storage_state_diff,
		test_state_override_storage_full_replacement,
		test_state_override_storage_full_clears_unspecified,
		test_state_override_storage_diff_preserves_unspecified,
		test_state_override_storage_multiple_slots,
		test_state_override_storage_full_empty_map_clears_all,
		test_state_override_storage_diff_zero_value,
		test_state_override_nonce,
		test_state_override_code_and_storage_combined,
		test_state_override_balance_on_from_account,
		test_state_override_multiple_accounts,
		test_state_override_combined_balance_and_code,
		test_state_override_does_not_persist,
		test_state_override_empty_set,
		test_state_override_storage_on_eoa_fails,
		test_state_override_balance_zero,
		test_state_override_trace_call,
		test_subscription_gap_filler_backfills_queued_range,
	);

	log::debug!(target: LOG_TARGET, "All tests completed successfully!");
	Ok(())
}

async fn test_transfer() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let initial_balance = client.get_balance(ethan.address(), Default::default()).await?;

	let value = 1_000_000_000_000_000_000_000u128.into();
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(ethan.address())
		.send()
		.await?;

	let receipt = tx.wait_for_receipt().await?;
	assert_eq!(
		Some(ethan.address()),
		receipt.to,
		"Receipt should have the correct contract address."
	);

	let balance = client.get_balance(ethan.address(), Default::default()).await?;
	assert_eq!(
		Some(value),
		balance.checked_sub(initial_balance),
		"Ethan {:?} {balance:?} should have increased by {value:?} from {initial_balance}.",
		ethan.address()
	);
	Ok(())
}

async fn test_deploy_and_call() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	// Balance transfer
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let initial_balance = client.get_balance(ethan.address(), Default::default()).await?;
	let value = 1_000_000_000_000_000_000_000u128.into();
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(ethan.address())
		.send()
		.await?;

	let receipt = tx.wait_for_receipt().await?;
	assert_eq!(
		Some(ethan.address()),
		receipt.to,
		"Receipt should have the correct contract address."
	);

	let balance = client.get_balance(ethan.address(), Default::default()).await?;
	assert_eq!(
		Some(value),
		balance.checked_sub(initial_balance),
		"Ethan {:?} {balance:?} should have increased by {value:?} from {initial_balance}.",
		ethan.address()
	);

	// Deploy contract
	let data = b"hello world".to_vec();
	let value = U256::from(5_000_000_000_000u128);
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let input = bytes.into_iter().chain(data.clone()).collect::<Vec<u8>>();
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone()).value(value).input(input).send().await?;
	let receipt = tx.wait_for_receipt().await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());
	assert_eq!(
		Some(contract_address),
		receipt.contract_address,
		"Contract should be deployed at {contract_address:?}."
	);

	let nonce_after_deploy =
		client.get_transaction_count(account.address(), Default::default()).await?;

	assert_eq!(nonce_after_deploy - nonce, U256::from(1), "Nonce should have increased by 1");

	let initial_balance = client.get_balance(contract_address, Default::default()).await?;
	assert_eq!(
		value, initial_balance,
		"Contract {contract_address:?} balance should be the same as the value sent ({value})."
	);

	// Call contract
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_address)
		.send()
		.await?;
	let receipt = tx.wait_for_receipt().await?;

	assert_eq!(
		Some(contract_address),
		receipt.to,
		"Receipt should have the correct contract address {contract_address:?}."
	);

	let balance = client.get_balance(contract_address, Default::default()).await?;
	assert_eq!(
		Some(value),
		balance.checked_sub(initial_balance),
		"Contract {contract_address:?} Balance {balance} should have increased from {initial_balance} by {value}."
	);

	// Balance transfer to contract
	let initial_balance = client.get_balance(contract_address, Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_address)
		.send()
		.await?;

	tx.wait_for_receipt().await?;

	let balance = client.get_balance(contract_address, Default::default()).await?;

	assert_eq!(
		Some(value),
		balance.checked_sub(initial_balance),
		"Balance {balance} should have increased from {initial_balance} by {value}."
	);
	Ok(())
}

/// Verify that a reverted transaction and a log-emitting transaction in the same block
/// produce correct, independent receipts: revert status and logs are not mixed up.
/// Also exercises eth_getTransactionByBlockNumberAndIndex for both transactions.
async fn test_receipt_mixed_revert_and_logs_same_block() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let deploy = |name, fixture_type| {
		let client = client.clone();
		let address = account.address();
		async move {
			let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(name, fixture_type)?;
			let nonce = client.get_transaction_count(address, Default::default()).await?;
			let tx = TransactionBuilder::new(client).input(bytes).send().await?;
			tx.wait_for_receipt().await?;
			Ok::<_, anyhow::Error>(create1(&address, nonce.try_into().unwrap()))
		}
	};

	let revert_contract =
		deploy("ok_trap_revert", pallet_revive_fixtures::FixtureType::Rust).await?;
	let event_contract = deploy("MultiEvent", pallet_revive_fixtures::FixtureType::Solc).await?;

	alloy::sol! {
		function emitMultiple(uint64 a, uint64 b);
		event Ping(uint64 value);
		event Pong(uint64 value);
	}

	// Get the current nonce and submit two transactions with descending nonces
	// so they land in the same block.
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;

	let revert_tx = TransactionBuilder::new(client.clone())
		.to(revert_contract)
		.input(vec![1, 0, 0, 0])
		.gas(U256::from(1_000_000))
		.nonce(nonce.saturating_add(U256::from(1)))
		.send()
		.await?;

	let emit_input = emitMultipleCall { a: 1, b: 2 }.abi_encode();
	let emit_tx = TransactionBuilder::new(client.clone())
		.to(event_contract)
		.input(emit_input)
		.gas(U256::from(1_000_000))
		.nonce(nonce)
		.send()
		.await?;

	let emit_receipt = emit_tx.wait_for_receipt().await?;
	let revert_receipt = revert_tx.wait_for_receipt_any().await?;

	// Both should be in the same block
	assert_eq!(
		emit_receipt.block_number, revert_receipt.block_number,
		"Both transactions should be in the same block"
	);

	// Reverted transaction: status=0, no logs
	assert!(!revert_receipt.is_success(), "Reverted call should have status=0x0");
	assert!(revert_receipt.logs.is_empty(), "Reverted call should produce no logs");

	// Successful transaction: status=1, 2 logs correctly attributed
	assert!(emit_receipt.is_success(), "emitMultiple call should succeed");
	assert_eq!(emit_receipt.logs.len(), 2, "Receipt should contain exactly 2 logs");
	assert_eq!(emit_receipt.logs[0].address, event_contract);
	assert_eq!(emit_receipt.logs[1].address, event_contract);

	let ping_sig = H256(Ping::SIGNATURE_HASH.0);
	let pong_sig = H256(Pong::SIGNATURE_HASH.0);
	assert_eq!(emit_receipt.logs[0].topics[0], ping_sig, "First log should be Ping");
	assert_eq!(emit_receipt.logs[1].topics[0], pong_sig, "Second log should be Pong");

	for log in &emit_receipt.logs {
		assert_eq!(
			log.transaction_hash, emit_receipt.transaction_hash,
			"Logs should belong to the emitting transaction"
		);
	}

	// Verify log data values
	let ping_data = &emit_receipt.logs[0].data.as_ref().unwrap().0;
	let pong_data = &emit_receipt.logs[1].data.as_ref().unwrap().0;
	let ping = Ping::abi_decode_data(ping_data).expect("decode Ping data");
	let pong = Pong::abi_decode_data(pong_data).expect("decode Pong data");
	assert_eq!(ping.0, 1, "Ping value should be 1");
	assert_eq!(pong.0, 2, "Pong value should be 2");

	// log_index must be monotonically increasing within the block
	assert!(
		emit_receipt.logs[0].log_index < emit_receipt.logs[1].log_index,
		"log_index values should be monotonically increasing"
	);

	let block_number = emit_receipt.block_number;
	// Verify eth_getTransactionByBlockNumberAndIndex for both
	let tx0 = client
		.get_transaction_by_block_number_and_index(
			BlockNumberOrTag::Number(block_number.as_u64()),
			emit_receipt.transaction_index,
		)
		.await?;
	assert_eq!(tx0.unwrap().hash, emit_receipt.transaction_hash);

	let tx1 = client
		.get_transaction_by_block_number_and_index(
			BlockNumberOrTag::Number(block_number.as_u64()),
			revert_receipt.transaction_index,
		)
		.await?;
	assert_eq!(tx1.unwrap().hash, revert_receipt.transaction_hash);

	Ok(())
}

async fn test_runtime_api_dry_run_addr_works() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let node_client = SharedResources::node_client().await;
	let account = Account::default();
	let origin: [u8; 32] = account.substrate_account().into();
	let data = b"hello world".to_vec();
	let value = 5_000_000_000_000u128;
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;

	let payload = subxt_client::runtime_apis().revive_api().instantiate(
		subxt::utils::AccountId32(origin),
		value,
		None,
		None,
		CodeV1::Upload(bytes).into(),
		data,
		None,
	);

	// runtime_api.at_latest() uses the latest finalized block, query nonce accordingly
	let nonce = client
		.get_transaction_count(account.address(), BlockNumberOrTag::Finalized.into())
		.await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());

	let res = node_client
		.at_current_block()
		.await?
		.runtime_apis()
		.call(payload)
		.await?
		.0
		.result
		.unwrap();

	assert_eq!(res.addr, contract_address);
	Ok(())
}

async fn test_invalid_transaction() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let ethan = Account::from(subxt_signer::eth::dev::ethan());

	let err = TransactionBuilder::new(client.clone())
		.value(U256::from(1_000_000_000_000u128))
		.to(ethan.address())
		.mutate(|tx| match tx {
			TransactionUnsigned::TransactionLegacyUnsigned(tx) => tx.chain_id = Some(42u32.into()),
			TransactionUnsigned::Transaction1559Unsigned(tx) => tx.chain_id = 42u32.into(),
			TransactionUnsigned::Transaction2930Unsigned(tx) => tx.chain_id = 42u32.into(),
			TransactionUnsigned::Transaction4844Unsigned(tx) => tx.chain_id = 42u32.into(),
			TransactionUnsigned::Transaction7702Unsigned(tx) => tx.chain_id = 42u32.into(),
		})
		.send()
		.await
		.unwrap_err();

	let call_err = unwrap_call_err!(err.source().unwrap());
	assert_eq!(call_err.message(), "Invalid Transaction");

	Ok(())
}

async fn get_evm_block_from_storage(
	node_client: &OnlineClient<SrcChainConfig>,
	node_rpc_client: &RpcClient,
	block_number: U256,
) -> anyhow::Result<BlockV1> {
	let block_hash: H256 = node_rpc_client
		.request("chain_getBlockHash", rpc_params![block_number])
		.await
		.unwrap();

	let query = subxt_client::storage().revive().ethereum_block();
	let at_block = node_client.at_block(block_hash).await?;
	let Some(value) = at_block.storage().try_fetch(query, ()).await? else {
		return Err(anyhow!("EVM block {block_hash:?} not found"));
	};
	let block = value.decode()?;
	Ok(block.0)
}

async fn test_evm_blocks_should_match() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let node_client = SharedResources::node_client().await;
	let node_rpc_client = RpcClient::from_url(SharedResources::node_rpc_url()).await?;

	// Deploy a contract to have some interesting blocks
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let value = U256::from(5_000_000_000_000u128);
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.input(bytes.to_vec())
		.send()
		.await?;

	let receipt = tx.wait_for_receipt().await?;
	let block_number = receipt.block_number;
	let block_hash = receipt.block_hash;
	log::trace!(target: LOG_TARGET, "block_number = {block_number:?}");
	log::trace!(target: LOG_TARGET, "tx hash = {:?}", tx.hash());

	let evm_block_from_storage =
		get_evm_block_from_storage(&node_client, &node_rpc_client, block_number).await?;

	// Fetch the block immediately (should come from storage EthereumBlock)
	let evm_block_from_rpc_by_number = client
		.get_block_by_number(BlockNumberOrTag::Number(block_number.as_u64()), false)
		.await?
		.expect("Block should exist");
	let evm_block_from_rpc_by_hash =
		client.get_block_by_hash(block_hash, false).await?.expect("Block should exist");

	assert!(
		matches!(evm_block_from_rpc_by_number.transactions, HashesOrTransactionInfosV1::Hashes(_)),
		"Block should not have hydrated transactions"
	);

	// All EVM blocks must match
	assert_eq!(evm_block_from_storage, evm_block_from_rpc_by_number, "EVM blocks should match");
	assert_eq!(evm_block_from_storage, evm_block_from_rpc_by_hash, "EVM blocks should match");

	Ok(())
}

async fn test_evm_blocks_hydrated_should_match() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	// Deploy a contract to have some transactions in the block
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let value = U256::from(5_000_000_000_000u128);
	let signer = Account::default();
	let signer_copy = Account::default();
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.signer(signer)
		.input(bytes.to_vec())
		.send()
		.await?;

	let receipt = tx.wait_for_receipt().await?;
	let block_number = receipt.block_number;
	let block_hash = receipt.block_hash;
	log::trace!(target: LOG_TARGET, "block_number = {block_number:?}");
	log::trace!(target: LOG_TARGET, "tx hash = {:?}", tx.hash());

	// Fetch the block with hydrated transactions via RPC (by number and by hash)
	let evm_block_from_rpc_by_number = client
		.get_block_by_number(BlockNumberOrTag::Number(block_number.as_u64()), true)
		.await?
		.expect("Block should exist");
	let evm_block_from_rpc_by_hash =
		client.get_block_by_hash(block_hash, true).await?.expect("Block should exist");

	// Both blocks should be identical
	assert_eq!(
		evm_block_from_rpc_by_number, evm_block_from_rpc_by_hash,
		"Hydrated EVM blocks should match"
	);

	// Verify transaction info
	let unsigned_tx = tx
		.generic_transaction()
		.try_into_unsigned()
		.expect("Transaction shall be converted");
	let signed_tx = signer_copy.sign_transaction(unsigned_tx);
	let expected_tx_info = receipt.transaction_info(signed_tx);

	let tx_info = if let HashesOrTransactionInfosV1::TransactionInfos(tx_infos) =
		evm_block_from_rpc_by_number.transactions
	{
		tx_infos[0].clone()
	} else {
		panic!("Expected hydrated transactions");
	};
	assert_eq!(expected_tx_info, tx_info, "TransationInfos should match");

	Ok(())
}

/// Verifies that `eth_getBlockReceipts` returns every receipt of a block, that querying the same
/// block by number and by hash yields the same receipts, and that an unknown block returns `null`.
async fn test_get_block_receipts() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let (bytecode, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let deploy_tx = TransactionRequest::default()
		.from(from)
		.input(AlloyBytes::from(bytecode).into())
		.create();

	// Act
	let receipt = provider.send_transaction(deploy_tx).await?.get_receipt().await?;
	let block_number = receipt.block_number.expect("Mined receipt has a block number");
	let block_hash = receipt.block_hash.expect("Mined receipt has a block hash");

	let by_number = provider
		.get_block_receipts(BlockId::number(block_number))
		.await?
		.expect("Block should have receipts");
	let by_hash = provider
		.get_block_receipts(BlockId::hash(block_hash))
		.await?
		.expect("Block should have receipts");
	let missing = provider.get_block_receipts(BlockId::hash(B256::from([0x42u8; 32]))).await?;

	// Assert
	let hashes_by_number = by_number.iter().map(|r| r.transaction_hash).collect::<Vec<_>>();
	let hashes_by_hash = by_hash.iter().map(|r| r.transaction_hash).collect::<Vec<_>>();
	assert_eq!(hashes_by_number, hashes_by_hash, "Receipts by number and by hash should match");
	assert!(
		by_number.iter().any(|r| r.transaction_hash == receipt.transaction_hash),
		"Block receipts should include the sent transaction"
	);
	assert!(
		by_number.iter().all(|r| r.block_hash == Some(block_hash)),
		"All receipts should belong to the queried block"
	);
	assert!(missing.is_none(), "Receipts for a non-existent block should be null");

	Ok(())
}

async fn test_block_hash_for_tag_with_proper_ethereum_block_hash_works() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	// Deploy a transaction to create a block with transactions
	let (bytes, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let value = U256::from(5_000_000_000_000u128);
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.input(bytes.to_vec())
		.send()
		.await?;

	let receipt = tx.wait_for_receipt().await?;
	let ethereum_block_hash = receipt.block_hash;

	log::trace!(target: LOG_TARGET, "Testing with Ethereum block hash: {ethereum_block_hash:?}");

	let block_by_hash = client
		.get_block_by_hash(ethereum_block_hash, false)
		.await?
		.expect("Block should exist");

	let account = Account::default();
	let balance = client
		.get_balance(account.address(), BlockId::hash(B256::from(ethereum_block_hash.0)))
		.await?;

	assert!(balance >= U256::zero(), "Balance should be retrievable with Ethereum hash");
	assert_eq!(block_by_hash.hash, ethereum_block_hash, "Block hash should match");

	Ok(())
}

async fn test_block_hash_for_tag_with_invalid_ethereum_block_hash_fails() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let fake_eth_hash = H256::from([0x42u8; 32]);

	log::trace!(target: LOG_TARGET, "Testing with fake Ethereum hash: {fake_eth_hash:?}");

	let account = Account::default();
	let result = client
		.get_balance(account.address(), BlockId::hash(B256::from(fake_eth_hash.0)))
		.await;

	assert!(result.is_err(), "Should fail with non-existent Ethereum hash");

	Ok(())
}

async fn test_block_hash_for_tag_with_block_number_works() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let block_number = client.block_number().await?;

	log::trace!(target: LOG_TARGET, "Testing with block number: {block_number}");

	let account = Account::default();
	let balance = client
		.get_balance(account.address(), BlockId::number(block_number.as_u64()))
		.await?;

	assert!(balance >= U256::zero(), "Balance should be retrievable with block number");
	Ok(())
}

/// The standard block tags every RPC entry point must accept.
const BLOCK_TAGS: [BlockNumberOrTag; 5] = [
	BlockNumberOrTag::Earliest,
	BlockNumberOrTag::Safe,
	BlockNumberOrTag::Finalized,
	BlockNumberOrTag::Pending,
	BlockNumberOrTag::Latest,
];

async fn test_block_hash_for_tag_with_block_tags_works() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	for tag in BLOCK_TAGS {
		let balance = client.get_balance(account.address(), tag.into()).await?;

		assert!(balance >= U256::zero(), "Balance should be retrievable with tag {tag:?}");
	}

	Ok(())
}

/// `eth_getLogs` must accept every standard block tag for `fromBlock`/`toBlock`.
async fn test_get_logs_with_block_tags_works() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	// Deploy a contract and trigger it to emit a log.
	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	TransactionBuilder::new(client.clone())
		.input(bytes.to_vec())
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());
	let emit_receipt = TransactionBuilder::new(client.clone())
		.value(U256::from(1_000_000_000_000u128))
		.to(contract_address)
		.send()
		.await?
		.wait_for_receipt()
		.await?;

	let emit_block = emit_receipt.block_number;

	wait_for_finalized_to_reach_best(&*client).await?;

	let logs_of = |results: FilterResults| match results {
		FilterResults::Logs(logs) => logs,
		FilterResults::Hashes(hashes) if hashes.is_empty() => Vec::new(),
		other => panic!("expected Logs from eth_getLogs, got: {other:?}"),
	};
	let has_emitted_log = |logs: &[Log]| logs.iter().any(|log| log.block_number == emit_block);

	// `<tag>..latest` range should span the emitted log.
	for from in BLOCK_TAGS {
		let logs = logs_of(
			client
				.get_logs(Some(Filter::new().from_block(from).to_block(BlockNumberOrTag::Latest)))
				.await
				.map_err(|err| anyhow::anyhow!("eth_getLogs {from:?}..latest failed: {err:?}"))?,
		);
		assert!(has_emitted_log(&logs), "{from:?}..latest should include the emitted log");
	}

	// `earliest..<tag>` range should span the emitted log for every tag except `earliest`.
	for to in BLOCK_TAGS {
		let logs = logs_of(
			client
				.get_logs(Some(Filter::new().from_block(BlockNumberOrTag::Earliest).to_block(to)))
				.await
				.map_err(|err| anyhow::anyhow!("eth_getLogs earliest..{to:?} failed: {err:?}"))?,
		);
		if matches!(to, BlockNumberOrTag::Earliest) {
			assert!(
				!has_emitted_log(&logs),
				"earliest..earliest (genesis) must not contain the log"
			);
		} else {
			assert!(has_emitted_log(&logs), "earliest..{to:?} should include the emitted log");
		}
	}

	Ok(())
}

/// Validate that all RPC methods accept the `earliest` block tag and return correct data.
async fn test_earliest_block_tag() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let tx = GenericTransactionV1 {
		from: Some(account.address()),
		to: Some(account.address()),
		..Default::default()
	};

	// eth_getBlockByNumber
	let block = client
		.get_block_by_number(BlockNumberOrTag::Earliest, false)
		.await?
		.expect("earliest block should exist");
	assert_eq!(block.number, U256::zero(), "earliest block number should be 0");

	// eth_getBalance
	let balance = client.get_balance(account.address(), BlockNumberOrTag::Earliest.into()).await?;
	assert!(balance > U256::zero(), "dev account should have a non-zero balance at genesis");

	// eth_getTransactionCount
	let nonce = client
		.get_transaction_count(account.address(), BlockNumberOrTag::Earliest.into())
		.await?;
	assert_eq!(nonce, U256::zero(), "nonce at genesis should be 0");

	// eth_getCode
	let code = client.get_code(account.address(), BlockNumberOrTag::Earliest.into()).await?;
	assert!(code.is_empty(), "EOA should have no code");

	// eth_getStorageAt
	let storage = client
		.get_storage_at(account.address(), U256::zero(), BlockNumberOrTag::Earliest.into())
		.await?;
	assert!(storage.0.iter().all(|&b| b == 0), "EOA should have zero storage");

	// eth_getBlockTransactionCountByNumber
	let tx_count = client
		.get_block_transaction_count_by_number(Some(BlockNumberOrTag::Earliest))
		.await?;
	assert_eq!(tx_count, Some(U256::zero()), "genesis block should have no transactions");

	// eth_getTransactionByBlockNumberAndIndex
	let tx_by_index = client
		.get_transaction_by_block_number_and_index(BlockNumberOrTag::Earliest, U256::zero())
		.await?;
	assert!(tx_by_index.is_none(), "genesis block should have no transactions");

	// eth_call
	let call_result =
		client.call(tx.clone(), Some(BlockNumberOrTag::Earliest.into()), None).await?;
	assert!(call_result.is_empty(), "calling an EOA should return empty bytes");

	// eth_estimateGas
	let gas = client.estimate_gas(tx.clone(), Some(BlockNumberOrTag::Earliest)).await?;
	assert!(gas > U256::zero(), "gas estimate should be non-zero");

	// eth_feeHistory
	let fee = client.fee_history(U256::from(1), BlockNumberOrTag::Earliest, None).await?;
	assert_eq!(fee.oldest_block, U256::zero(), "feeHistory oldest_block should be 0");
	assert!(!fee.base_fee_per_gas.is_empty(), "feeHistory should include base fee");

	// eth_getLogs
	let filter = Filter::new()
		.from_block(BlockNumberOrTag::Earliest)
		.to_block(BlockNumberOrTag::Earliest);
	let logs = client.get_logs(Some(filter)).await?;
	assert_eq!(logs, FilterResults::default(), "genesis block should have no logs");

	// debug_traceBlockByNumber
	let traces =
		DebugRpcClient::trace_block_by_number(&*client, BlockNumberOrTag::Earliest, None).await?;
	assert!(traces.is_empty(), "genesis block should have no traces");

	// debug_traceCall
	let trace =
		DebugRpcClient::trace_call(&*client, tx.clone(), BlockNumberOrTag::Earliest.into(), None)
			.await?;
	assert!(
		matches!(trace, TraceV1::Call(_) | TraceV1::Execution(_)),
		"traceCall should return a trace"
	);

	Ok(())
}

async fn test_multiple_transactions_in_block() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let num_transactions = 20;
	let alith = Account::default();
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let amount = U256::from(1_000_000_000_000_000_000u128);

	// Prepare EVM transfer transactions
	let transactions =
		prepare_evm_transactions(client.clone(), alith, ethan.address(), amount, num_transactions)
			.await?;

	// Submit all transactions
	let submitted_txs = submit_evm_transactions(transactions).await?;
	let tx_hashes: Vec<H256> = submitted_txs.iter().map(|(hash, _, _)| *hash).collect();
	log::trace!(target: LOG_TARGET, "Submitted {} transactions", submitted_txs.len());

	// All transactions should be included in the same block since nonces are in descending order
	let first_receipt = submitted_txs[0].2.wait_for_receipt().await?;

	// Fetch and verify block contains all transactions
	verify_transactions_in_single_block(&client, first_receipt.block_number, &tx_hashes).await?;
	Ok(())
}

async fn test_mixed_evm_substrate_transactions() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let node_client = SharedResources::node_client().await;
	let num_evm_txs = 10;
	let num_substrate_txs = 7;

	let alith = Account::default();
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let amount = U256::from(500_000_000_000_000_000u128);

	// Prepare EVM transactions
	log::trace!(target: LOG_TARGET, "Creating {num_evm_txs} EVM transfer transactions");
	let evm_transactions =
		prepare_evm_transactions(client.clone(), alith, ethan.address(), amount, num_evm_txs)
			.await?;

	// Prepare substrate transactions (simple remarks)
	log::trace!(target: LOG_TARGET, "Creating {num_substrate_txs} substrate remark transactions");
	let alice_signer = subxt_signer::sr25519::dev::alice();

	let substrate_txs =
		prepare_substrate_transactions(&node_client, &alice_signer, num_substrate_txs).await?;

	log::trace!(target: LOG_TARGET, "Submitting {num_evm_txs} EVM and {num_substrate_txs} substrate transactions");

	// Submit EVM transactions
	let evm_submitted = submit_evm_transactions(evm_transactions).await?;
	let evm_tx_hashes: Vec<H256> = evm_submitted.iter().map(|(hash, _, _)| *hash).collect();

	// Submit substrate transactions
	let substrate_futures = submit_substrate_transactions(substrate_txs).await;

	// Wait for first EVM receipt and all substrate transactions in parallel
	let (evm_first_receipt_result, _substrate_results) = tokio::join!(
		async { evm_submitted[0].2.wait_for_receipt().await },
		futures::future::join_all(substrate_futures)
	);
	// Handle the EVM receipt result
	let evm_first_receipt = evm_first_receipt_result?;

	// Fetch and verify block contains all transactions
	verify_transactions_in_single_block(&client, evm_first_receipt.block_number, &evm_tx_hashes)
		.await?;

	Ok(())
}

async fn test_runtime_pallets_address_upload_code() -> anyhow::Result<()> {
	let client = Arc::new(SharedResources::client().await);
	let node_client = SharedResources::node_client().await;
	let node_rpc_client = RpcClient::from_url(SharedResources::node_rpc_url()).await?;

	let (bytecode, _) = pallet_revive_fixtures::compile_module("dummy")?;
	let signer = Account::default();

	// Helper function to get substrate block hash from EVM block number
	let get_substrate_block_hash = |block_number: U256| {
		let rpc_client = node_rpc_client.clone();
		async move {
			rpc_client
				.request::<sp_core::H256>("chain_getBlockHash", rpc_params![block_number])
				.await
		}
	};

	// Step 1: Encode the Substrate upload_code call
	let upload_call = subxt::dynamic::tx(
		"Revive",
		"upload_code",
		vec![
			subxt::dynamic::Value::from_bytes(&bytecode),
			subxt::dynamic::Value::u128(u128::max_value()), // storage_deposit_limit
		],
	);
	let encoded_call = node_client.tx().await?.call_data(&upload_call)?;

	// Step 2: Send the encoded call to RUNTIME_PALLETS_ADDR
	let tx = TransactionBuilder::new(client.clone())
		.signer(signer.clone())
		.to(pallet_revive::RUNTIME_PALLETS_ADDR)
		.input(encoded_call.clone())
		.send()
		.await?;

	// Step 3: Wait for receipt
	let receipt = tx.wait_for_receipt().await?;

	// Step 4: Verify transaction was successful
	assert_eq!(
		receipt.status.unwrap_or(U256::zero()),
		U256::one(),
		"Transaction should be successful"
	);

	// Step 5: Verify the code was actually uploaded
	let code_hash = H256(sp_io::hashing::keccak_256(&bytecode));
	let query = subxt_client::storage().revive().pristine_code();
	let block_hash: sp_core::H256 = get_substrate_block_hash(receipt.block_number).await?;
	let at_block = node_client.at_block(block_hash).await?;
	let stored_code = at_block.storage().try_fetch(query, (code_hash,)).await?;
	let stored_code = stored_code.map(|v| v.decode()).transpose()?;
	assert!(stored_code.is_some(), "Code with hash {code_hash:?} should exist in storage");
	assert_eq!(stored_code.unwrap(), bytecode, "Stored code should match the uploaded bytecode");

	Ok(())
}

/// Verify that Alloy can subscribe to `newHeads` and receive the transaction's block header.
async fn test_new_heads_subscription_delivers_matching_header_via_alloy() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let provider = ProviderBuilder::new()
		.connect_ws(WsConnect::new("ws://localhost:45788"))
		.await?;
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let value = U256::from(1_000_000_000_000u128);

	let mut sub = provider.subscribe_blocks().await?;

	// Act
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(ethan.address())
		.send()
		.await?;
	let receipt = tx.wait_for_receipt().await?;

	let header = tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.recv_result())
		.await
		.expect("Timed out waiting for newHeads notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Alloy should deserialize the newHeads notification");

	let block = client
		.get_block_by_number(BlockNumberOrTag::Number(header.number), false)
		.await?
		.expect("Block should exist");

	// Assert
	assert_eq!(U256::from(header.number), receipt.block_number);
	assert_eq!(H256(header.hash.0), receipt.block_hash);
	assert_ne!(header.parent_hash, B256::ZERO, "Parent hash should not be zero");

	let header = serde_json::from_value::<BlockHeader>(serde_json::to_value(header)?)?;
	let expected_header = BlockHeader::from(block);
	assert_eq!(
		header, expected_header,
		"Subscription header should match the block header from RPC"
	);

	drop(sub);

	Ok(())
}

/// Verify that subscribing to `logs` delivers a log matching the corresponding
/// log fetched via `eth_getLogs` after a contract emits an event.
async fn test_subscribe_logs() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt = tx.wait_for_receipt().await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());
	assert_eq!(Some(contract_address), receipt.contract_address);

	let mut sub = client.eth_subscribe(SubscriptionKind::Logs, None).await?;

	// Act
	let value = U256::from(1_000_000_000_000u128);
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_address)
		.send()
		.await?;
	let call_receipt = tx.wait_for_receipt().await?;

	let notification = tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for logs notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error");

	let log = match notification {
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	let filter = Filter::new().at_block_hash(B256::from(call_receipt.block_hash.0));
	let rpc_logs = client.get_logs(Some(filter)).await?;
	let rpc_logs: Vec<Log> = match rpc_logs {
		FilterResults::Logs(logs) => logs,
		other => panic!("Expected Logs from eth_getLogs, got: {other:?}"),
	};

	// Assert
	let event_signature = H256(sp_io::hashing::keccak_256(b"Received(address,uint256)"));
	assert_eq!(log.address, contract_address, "Log address should be the contract address");
	assert!(!log.topics.is_empty(), "Log should have at least one topic");
	assert_eq!(log.topics[0], event_signature, "First topic should be the event signature hash");
	assert_eq!(
		log.block_hash, call_receipt.block_hash,
		"Log block hash should match receipt block hash"
	);
	assert_eq!(
		log.transaction_hash, call_receipt.transaction_hash,
		"Log tx hash should match receipt tx hash"
	);
	assert!(rpc_logs.contains(&log), "Subscription log should match eth_getLogs result");

	drop(sub);
	Ok(())
}

/// Verify that subscribing to `logs` with an address filter only delivers logs
/// emitted from the specified contract address.
async fn test_subscribe_logs_with_address_filter() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt = tx.wait_for_receipt().await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());
	assert_eq!(Some(contract_address), receipt.contract_address);

	let options = SubscriptionOptions::LogsOptions {
		address: Some(BoundedOneOrMany::One(contract_address)),
		topics: None,
	};
	let mut sub = client.eth_subscribe(SubscriptionKind::Logs, Some(options)).await?;

	// Act
	let value = U256::from(1_000_000_000_000u128);
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_address)
		.send()
		.await?;
	tx.wait_for_receipt().await?;

	let notification = tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for logs notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error");

	let log = match notification {
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	// Assert
	assert_eq!(log.address, contract_address, "Log address should match the filter address");

	drop(sub);
	Ok(())
}

/// Verify that subscribing to `logs` with a topic filter delivers logs whose
/// first topic matches the computed event signature hash.
async fn test_subscribe_logs_with_topic_filter() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt = tx.wait_for_receipt().await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());
	assert_eq!(Some(contract_address), receipt.contract_address);

	let event_signature = H256(sp_io::hashing::keccak_256(b"Received(address,uint256)"));
	let options = SubscriptionOptions::LogsOptions {
		address: None,
		topics: Some(
			BoundedVec::try_from(vec![Some(BoundedOneOrMany::One(event_signature))])
				.expect("Single topic filter is within bounds"),
		),
	};
	let mut sub = client.eth_subscribe(SubscriptionKind::Logs, Some(options)).await?;

	// Act
	let value = U256::from(1_000_000_000_000u128);
	let tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_address)
		.send()
		.await?;
	tx.wait_for_receipt().await?;

	let notification = tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for logs notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error");

	let log = match notification {
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	// Assert
	assert_eq!(
		log.topics[0], event_signature,
		"First topic should match the computed event signature"
	);
	assert_eq!(log.address, contract_address, "Log should come from the deployed contract");

	drop(sub);
	Ok(())
}

/// Verify that sending two sequential transactions yields two `newHeads`
/// notifications whose block numbers are increasing and whose parent hashes
/// chain correctly (the second header's `parent_hash` equals the first
/// header's `hash`).
async fn test_subscribe_new_heads_multiple_blocks() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let value = U256::from(1_000_000_000_000u128);

	let mut sub = client.eth_subscribe(SubscriptionKind::NewBlockHeaders, None).await?;

	// Act
	let tx1 = TransactionBuilder::new(client.clone())
		.value(value)
		.to(ethan.address())
		.send()
		.await?;
	tx1.wait_for_receipt().await?;

	let tx2 = TransactionBuilder::new(client.clone())
		.value(value)
		.to(ethan.address())
		.send()
		.await?;
	tx2.wait_for_receipt().await?;

	let header1 = match tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for first newHeads notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error")
	{
		SubscriptionItem::BlockHeader(h) => h,
		other => panic!("Expected BlockHeader, got: {other:?}"),
	};

	let header2 = match tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for second newHeads notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error")
	{
		SubscriptionItem::BlockHeader(h) => h,
		other => panic!("Expected BlockHeader, got: {other:?}"),
	};

	// Assert
	assert!(
		header2.number > header1.number,
		"Second block number ({}) should be greater than first ({})",
		header2.number,
		header1.number,
	);
	assert_eq!(
		header2.parent_hash, header1.hash,
		"Second header's parent_hash should equal first header's hash"
	);

	drop(sub);
	Ok(())
}

/// Verify that a `logs` subscription with an address filter does NOT deliver
/// logs emitted by a different contract. Two `SimpleReceiver` contracts are
/// deployed. The subscription is filtered to contract A's address. An event
/// is triggered on contract B first, then on contract A. The first
/// notification received must be from contract A, proving B's log was
/// correctly filtered out.
async fn test_subscribe_logs_address_filter_excludes_non_matching() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;

	let nonce_a = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx_a = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt_a = tx_a.wait_for_receipt().await?;
	let contract_a = create1(&account.address(), nonce_a.try_into().unwrap());
	assert_eq!(Some(contract_a), receipt_a.contract_address);

	let nonce_b = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx_b = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt_b = tx_b.wait_for_receipt().await?;
	let contract_b = create1(&account.address(), nonce_b.try_into().unwrap());
	assert_eq!(Some(contract_b), receipt_b.contract_address);
	assert_ne!(contract_a, contract_b, "The two contracts must have different addresses");

	let options = SubscriptionOptions::LogsOptions {
		address: Some(BoundedOneOrMany::One(contract_a)),
		topics: None,
	};
	let mut sub = client.eth_subscribe(SubscriptionKind::Logs, Some(options)).await?;

	// Act
	let value = U256::from(1_000_000_000_000u128);
	let tx_b_call = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_b)
		.send()
		.await?;
	tx_b_call.wait_for_receipt().await?;

	let tx_a_call = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_a)
		.send()
		.await?;
	tx_a_call.wait_for_receipt().await?;

	let notification = tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for logs notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error");

	let log = match notification {
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	// Assert
	assert_eq!(log.address, contract_a, "Log must come from contract A, not contract B");
	assert_ne!(log.address, contract_b, "Log should NOT come from contract B");

	drop(sub);
	Ok(())
}

/// Verify that a `logs` subscription with a multiple-address filter (OR
/// semantics) delivers logs from both specified contracts. Two
/// `SimpleReceiver` contracts are deployed and the subscription filter
/// includes both addresses. An event is triggered on each contract and
/// both logs must be received.
async fn test_subscribe_logs_with_multiple_addresses_filter() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;

	let nonce_a = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx_a = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt_a = tx_a.wait_for_receipt().await?;
	let contract_a = create1(&account.address(), nonce_a.try_into().unwrap());
	assert_eq!(Some(contract_a), receipt_a.contract_address);

	let nonce_b = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx_b = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt_b = tx_b.wait_for_receipt().await?;
	let contract_b = create1(&account.address(), nonce_b.try_into().unwrap());
	assert_eq!(Some(contract_b), receipt_b.contract_address);

	let options = SubscriptionOptions::LogsOptions {
		address: Some(BoundedOneOrMany::Many(
			BoundedVec::try_from(vec![contract_a, contract_b])
				.expect("Two addresses is within bounds"),
		)),
		topics: None,
	};
	let mut sub = client.eth_subscribe(SubscriptionKind::Logs, Some(options)).await?;

	// Act
	let value = U256::from(1_000_000_000_000u128);
	let tx_a_call = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_a)
		.send()
		.await?;
	tx_a_call.wait_for_receipt().await?;

	let tx_b_call = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_b)
		.send()
		.await?;
	tx_b_call.wait_for_receipt().await?;

	let log1 = match tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for first log notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error")
	{
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	let log2 = match tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for second log notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error")
	{
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	// Assert
	let mut received_addresses = vec![log1.address, log2.address];
	received_addresses.sort();
	let mut expected_addresses = vec![contract_a, contract_b];
	expected_addresses.sort();
	assert_eq!(received_addresses, expected_addresses, "Should receive one log from each contract");

	drop(sub);
	Ok(())
}

/// Verify that a plain ETH transfer between EOAs (which emits no events)
/// does not produce a log subscription notification. The subscription must
/// only deliver the log triggered by the subsequent contract call.
async fn test_subscribe_logs_no_event_transaction_ignored() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();
	let ethan = Account::from(subxt_signer::eth::dev::ethan());

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"SimpleReceiver",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;
	let nonce = client.get_transaction_count(account.address(), Default::default()).await?;
	let tx = TransactionBuilder::new(client.clone()).input(bytes.to_vec()).send().await?;
	let receipt = tx.wait_for_receipt().await?;
	let contract_address = create1(&account.address(), nonce.try_into().unwrap());
	assert_eq!(Some(contract_address), receipt.contract_address);

	let mut sub = client.eth_subscribe(SubscriptionKind::Logs, None).await?;

	// Act
	let value = U256::from(1_000_000_000_000u128);
	let plain_tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(ethan.address())
		.send()
		.await?;
	plain_tx.wait_for_receipt().await?;

	let contract_tx = TransactionBuilder::new(client.clone())
		.value(value)
		.to(contract_address)
		.send()
		.await?;
	contract_tx.wait_for_receipt().await?;

	let notification = tokio::time::timeout(tokio::time::Duration::from_secs(10), sub.next())
		.await
		.expect("Timed out waiting for log notification")
		.expect("Subscription stream ended unexpectedly")
		.expect("Subscription returned an error");

	let log = match notification {
		SubscriptionItem::Log(log) => log,
		other => panic!("Expected Log, got: {other:?}"),
	};

	// Assert
	assert_eq!(
		log.address, contract_address,
		"First log notification must come from the contract call, not the plain transfer"
	);
	assert_eq!(
		log.transaction_hash,
		contract_tx.hash(),
		"Log transaction hash must match the contract call, not the plain transfer"
	);

	drop(sub);
	Ok(())
}

/// Verify that calling `eth_subscribe("newHeads")` with `LogsOptions`
/// returns an error, since `newHeads` does not accept filter options.
async fn test_subscribe_with_invalid_params_rejected() -> anyhow::Result<()> {
	// Arrange
	let client = Arc::new(SharedResources::client().await);

	let options = SubscriptionOptions::LogsOptions {
		address: Some(BoundedOneOrMany::One(Account::default().address())),
		topics: None,
	};

	// Act
	let result = client.eth_subscribe(SubscriptionKind::NewBlockHeaders, Some(options)).await;

	// Assert
	assert!(result.is_err(), "newHeads with LogsOptions should be rejected");

	Ok(())
}

async fn test_estimate_gas_of_contract_with_consume_all_gas() -> anyhow::Result<()> {
	// Arrange
	let code = pallet_revive_fixtures::compile_module_with_type(
		"ContractWithConsumeAllGas",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?
	.0;
	let client = Arc::new(SharedResources::client().await);
	let account = Account::default();

	let receipt = TransactionBuilder::new(client.clone())
		.input(code)
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	let contract_address = receipt
		.contract_address
		.expect("Expected the transaction to publish a contract");

	// Act
	let test_function_selector = [0xf8, 0xa8, 0xfd, 0x6d].to_vec();
	let transaction = GenericTransactionV1 {
		from: Some(account.address()),
		input: test_function_selector.into(),
		to: Some(contract_address),
		chain_id: Some(client.chain_id().await?),
		nonce: Some(client.get_transaction_count(account.address(), Default::default()).await?),
		r#type: Some(0u8.into()),
		..Default::default()
	};
	let dry_run_result = client.estimate_gas(transaction, None).await;

	// Assert
	dry_run_result.expect("Dry run of this transaction must succeed");

	Ok(())
}

async fn test_gas_estimation_for_contract_requiring_binary_search() -> anyhow::Result<()> {
	// Arrange
	let code = pallet_revive_fixtures::compile_module_with_type(
		"ContractRequiringBinarySearchForGasEstimation",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?
	.0;
	let client = Arc::new(SharedResources::client().await);

	let receipt = TransactionBuilder::new(client.clone())
		.input(code)
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	let contract_address = receipt
		.contract_address
		.expect("Expected the transaction to publish a contract");

	// Act
	let main_function_selector = [0xdf, 0xfe, 0xad, 0xd0];
	let receipt = TransactionBuilder::new(client.clone())
		.to(contract_address)
		.input(main_function_selector.to_vec())
		.send()
		.await?
		.wait_for_receipt()
		.await?;

	// Assert
	assert!(receipt.is_success());

	Ok(())
}

/// Test that deploys and calls the Fibonacci contract via Substrate APIs works
async fn test_fibonacci_call_via_runtime_api() -> anyhow::Result<()> {
	use pallet_revive::precompiles::alloy::sol_types::SolCall;
	use pallet_revive_fixtures::Fibonacci;

	let (bytes, _) = pallet_revive_fixtures::compile_module_with_type(
		"Fibonacci",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;

	let node_client =
		OnlineClient::<SrcChainConfig>::from_url(SharedResources::node_rpc_url()).await?;
	let signer = subxt_signer::sr25519::dev::alice();
	let origin: [u8; 32] = signer.public_key().0;

	// Deploy the Fibonacci contract via Substrate API
	log::trace!(target: LOG_TARGET, "Deploying Fibonacci contract via Substrate API");
	let dry_run_result = node_client
		.at_current_block()
		.await?
		.runtime_apis()
		.call(subxt_client::runtime_apis().revive_api().instantiate(
			subxt::utils::AccountId32(origin),
			0u128, // value
			None,  // gas_limit
			None,  // storage_deposit_limit
			CodeV1::Upload(bytes.clone()).into(),
			vec![], // data (constructor args)
			None,   // salt
		))
		.await;

	assert!(dry_run_result.is_ok(), "Dry-run instantiate failed: {dry_run_result:?}");
	let dry_run = dry_run_result.unwrap().0;
	let instantiate_result = dry_run.result.expect("Dry-run should succeed");

	log::trace!(
		target: LOG_TARGET,
		"Dry-run succeeded: address: {:?}, gas_consumed: {:?}, weight_required: {:?}",
		instantiate_result.addr,
		dry_run.gas_consumed,
		dry_run.weight_required
	);

	// Now submit the actual instantiate extrinsic
	let events = node_client
		.tx()
		.await?
		.sign_and_submit_then_watch_default(
			&subxt_client::tx().revive().instantiate_with_code(
				0u128,                          // value
				dry_run.weight_required.into(), // weight_limit from dry-run
				u128::MAX,                      // storage_deposit_limit
				bytes,                          // code
				vec![],                         // data
				None,                           // salt
			),
			&subxt_signer::sr25519::dev::alice(),
		)
		.await?
		.wait_for_finalized_success()
		.await?;

	// Extract the contract address from the Instantiated event
	let instantiated_event = events
		.find_first::<subxt_client::revive::events::Instantiated>()
		.expect("Instantiated event should be present")?;

	let contract_address = instantiated_event.contract;
	log::trace!(target: LOG_TARGET, "Contract deployed via Substrate at: {contract_address:?}");

	// Verify that the dry-run predicted address matches the actual deployed address
	assert_eq!(
		instantiate_result.addr, contract_address,
		"Dry-run predicted address should match actual deployed address"
	);

	// Call the deployed contract using runtime API
	let call_data = Fibonacci::fibCall { n: 3u64 }.abi_encode();
	let call_payload = subxt_client::runtime_apis().revive_api().call(
		subxt::utils::AccountId32(origin),
		contract_address,
		0u128, // value
		None,  // gas_limit
		None,  // storage_deposit_limit
		call_data,
	);

	let result = node_client.at_current_block().await?.runtime_apis().call(call_payload).await;

	assert!(result.is_ok(), "Contract call failed: {result:?}");
	let call_result = result.unwrap().0;
	let exec_result = call_result.result.expect("fib(3) should succeed");

	let decoded = Fibonacci::fibCall::abi_decode_returns(&exec_result.data)
		.expect("Failed to decode return value");
	assert_eq!(decoded, 2u64, "fib(3) should return 2");

	// Verify that large Fibonacci values run out of gas
	let call_data = Fibonacci::fibCall { n: 100u64 }.abi_encode();
	let call_payload = subxt_client::runtime_apis().revive_api().call(
		subxt::utils::AccountId32(origin),
		contract_address,
		0u128, // value
		None,  // gas_limit
		None,  // storage_deposit_limit
		call_data,
	);

	let result = node_client.at_current_block().await?.runtime_apis().call(call_payload).await;
	assert!(result.is_ok(), "Runtime API call failed: {result:?}");
	let call_result = result.unwrap();
	assert!(call_result.result.is_err(), "fib(100) should run out of gas");

	Ok(())
}

async fn test_trace_block_returns_v1_trace_on_v1_input_and_v2_trace_on_v2_input()
-> anyhow::Result<()> {
	use pallet_revive_fixtures::Host;

	type SubstrateTracingBlock = sp_runtime::generic::Block<
		sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
		sp_runtime::OpaqueExtrinsic,
	>;

	let client = Arc::new(SharedResources::client().await);
	let node_client = SharedResources::node_client().await;

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Host",
		pallet_revive_fixtures::FixtureType::Solc,
	)?;
	let deploy_receipt = TransactionBuilder::new(client.clone())
		.input(code)
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	let contract_address = deploy_receipt
		.contract_address
		.ok_or_else(|| anyhow!("deployment should return a contract address"))?;

	let receipt = TransactionBuilder::new(client)
		.to(contract_address)
		.input(Host::HostCalls::logOps(Host::logOpsCall {}).abi_encode())
		.gas(U256::from(1_000_000))
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	assert!(receipt.is_success());
	assert_eq!(receipt.logs.len(), 5);

	let receipt_block_number = u32::try_from(receipt.block_number)
		.map_err(|_| anyhow!("receipt block number should fit in u32"))?;
	let subxt_block = node_client.at_block(receipt_block_number).await?;
	assert_eq!(subxt_block.block_number(), u64::from(receipt_block_number));

	let subxt_header = subxt_block.block_header().await?;
	let parent_hash = subxt_header.parent_hash;
	let header = codec::Decode::decode(&mut &codec::Encode::encode(&subxt_header)[..])?;
	let extrinsics = subxt_block
		.extrinsics()
		.fetch()
		.await?
		.iter()
		.map(|extrinsic| {
			sp_runtime::OpaqueExtrinsic::try_from_encoded_extrinsic(extrinsic?.bytes())
				.map_err(anyhow::Error::from)
		})
		.collect::<Result<Vec<_>, _>>()?;
	let block = SubstrateTracingBlock { header, extrinsics };
	let config = TracerTypeV1::CallTracer(Some(CallTracerConfigV1 {
		with_logs: true,
		only_top_call: false,
	}));

	let v1_input = TraceBlockVersionedInputPayload::V1(TraceBlockInputPayloadV1 {
		block: subxt::utils::Static(block.clone()),
		config: config.clone(),
	});
	let v1_payload = subxt_client::runtime_apis()
		.revive_api()
		.trace_block_versioned(subxt::utils::Static(v1_input))
		.unvalidated();
	let v1_output = node_client
		.at_block(parent_hash)
		.await?
		.runtime_apis()
		.call(v1_payload)
		.await?
		.0;
	let TraceBlockVersionedOutputPayload::V1(v1_output) = v1_output else {
		return Err(anyhow!("V1 trace_block input should return V1 output"));
	};
	let (_, trace_v1) = v1_output
		.traces
		.into_iter()
		.find(|(_, trace)| matches!(trace, TraceV1::Call(call) if !call.logs.is_empty()))
		.ok_or_else(|| anyhow!("V1 output should include a call trace with logs"))?;
	let TraceV1::Call(call_v1) = trace_v1 else {
		return Err(anyhow!("V1 output should include a call trace"));
	};
	assert_eq!(call_v1.logs.len(), 5);
	assert!(serde_json::to_value(&call_v1.logs[0])?.get("index").is_none());

	let v2_input = TraceBlockVersionedInputPayload::V2(TraceBlockInputPayloadV2 {
		block: subxt::utils::Static(block),
		config,
	});
	let v2_payload = subxt_client::runtime_apis()
		.revive_api()
		.trace_block_versioned(subxt::utils::Static(v2_input))
		.unvalidated();
	let v2_output = node_client
		.at_block(parent_hash)
		.await?
		.runtime_apis()
		.call(v2_payload)
		.await?
		.0;
	let TraceBlockVersionedOutputPayload::V2(v2_output) = v2_output else {
		return Err(anyhow!("V2 trace_block input should return V2 output"));
	};
	let (_, entry_v2) = v2_output
		.entries
		.into_iter()
		.find(
			|(_, entry)| matches!(entry, TraceEntryV1::Traced(TraceV2::Call(call)) if !call.logs.is_empty()),
		)
		.ok_or_else(|| anyhow!("V2 output should include a call trace with logs"))?;
	let TraceEntryV1::Traced(TraceV2::Call(call_v2)) = entry_v2 else {
		return Err(anyhow!("V2 output should include a call trace"));
	};
	let indexes = call_v2.logs.iter().map(|log| log.index).collect::<Vec<_>>();
	assert_eq!(indexes, vec![3, 4, 5, 6, 7]);

	Ok(())
}

async fn test_gas_estimation_with_no_funds_no_gas_specified() -> anyhow::Result<()> {
	// Arrange
	let code = pallet_revive_fixtures::compile_module_with_type(
		"ContractWithConsumeAllGas",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?
	.0;
	let client = Arc::new(SharedResources::client().await);
	let account = Account::from(Keypair::from_seed([0xFF; 16].as_slice()).unwrap());

	let receipt = TransactionBuilder::new(client.clone())
		.input(code)
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	let contract_address = receipt
		.contract_address
		.expect("Expected the transaction to publish a contract");

	// Act
	let test_function_selector = [0xf8, 0xa8, 0xfd, 0x6d].to_vec();
	let transaction = GenericTransactionV1 {
		from: Some(account.address()),
		input: test_function_selector.into(),
		to: Some(contract_address),
		chain_id: Some(client.chain_id().await?),
		nonce: Some(client.get_transaction_count(account.address(), Default::default()).await?),
		r#type: Some(0u8.into()),
		..Default::default()
	};
	let dry_run_result = client.estimate_gas(transaction, None).await;

	// Assert
	dry_run_result.expect("Expected this dry run to succeed");

	Ok(())
}

/// Submit `count` EVM transfer transactions and wait for inclusion.
async fn submit_evm_transfers(count: usize) -> anyhow::Result<()> {
	let ws_client = Arc::new(SharedResources::client().await);
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	for _ in 0..count {
		let tx = TransactionBuilder::new(Arc::clone(&ws_client))
			.signer(Account::default())
			.value(U256::from(1_000_000_000_000u128))
			.to(ethan.address())
			.send()
			.await?;
		tx.wait_for_receipt().await?;
	}
	Ok(())
}

/// Create a [`Client`] for block-sync testing.
///
/// Connects to the same dev-node that [`SharedResources`] started, but uses its own
/// in-memory SQLite database so that sync labels written by the test do not interfere
/// with the eth-rpc server's internal database (and vice versa).
async fn create_sync_test_client() -> anyhow::Result<Client> {
	let (client, _gap_fill_rx) = create_sync_test_client_with_subscription_gap_queue().await?;
	Ok(client)
}

async fn create_sync_test_client_with_subscription_gap_queue()
-> anyhow::Result<(Client, mpsc::Receiver<GapFillRequest>)> {
	use sc_cli::{RPC_DEFAULT_MAX_REQUEST_SIZE_MB, RPC_DEFAULT_MAX_RESPONSE_SIZE_MB};

	let node_url = SharedResources::node_rpc_url();
	let max_request_size = RPC_DEFAULT_MAX_REQUEST_SIZE_MB * 1024 * 1024;
	let max_response_size = RPC_DEFAULT_MAX_RESPONSE_SIZE_MB * 1024 * 1024;
	let (api, rpc_client, rpc) = connect(node_url, max_request_size, max_response_size).await?;
	let block_provider = SubxtBlockInfoProvider::new(api.clone(), rpc.clone()).await?;

	let pool = SqlitePoolOptions::new()
		.max_connections(1)
		.idle_timeout(None)
		.max_lifetime(None)
		.connect_with(SqliteConnectOptions::new().in_memory(true))
		.await?;

	let runtime_api_provider = VersionAwareRuntimeApiProvider::new(api.clone(), rpc_client.clone());
	let receipt_extractor = ReceiptExtractor::new(runtime_api_provider.clone()).await?;
	let receipt_provider = ReceiptProvider::new(
		DbContext::new(pool, DbContext::DEFAULT_MAX_VARIABLE_NUMBER),
		block_provider.clone(),
		receipt_extractor,
		None,
	)
	.await?;

	let (subscription_gap_queue, gap_fill_rx) = SubscriptionGapQueue::new();
	let client = Client::new(
		api,
		rpc_client,
		rpc,
		block_provider,
		receipt_provider,
		true,
		subscription_gap_queue,
		runtime_api_provider,
	)
	.await?;
	Ok((client, gap_fill_rx))
}

/// Fresh sync: labels, hash mappings, and re-sync idempotency.
async fn test_block_sync_fresh() -> anyhow::Result<()> {
	use crate::block_sync::SyncCheckpoint;

	// Submit a transaction so the chain has at least one block with EVM data to sync.
	submit_evm_transfers(1).await?;

	let client = create_sync_test_client().await?;

	// Fresh DB — sync_state table should be empty.
	for label in [SyncLabel::Tail, SyncLabel::Head] {
		assert!(
			client.receipt_provider().get_sync_label(label).await?.is_none(),
			"sync_state[{label}] should be absent on fresh DB"
		);
	}
	for label in [ChainMetadata::Genesis, ChainMetadata::FirstEvmBlock] {
		assert!(
			client.receipt_provider().get_sync_label(label).await?.is_none(),
			"sync_state[{label}] should be absent on fresh DB"
		);
	}

	// Capture finalized before sync — Head will be set to this snapshot.
	let finalized_before_sync = client.latest_finalized_block().await.block_number();

	// Run the full backward sync.
	client.sync_backward().await?;

	// Genesis label must match the chain.
	let genesis = client
		.receipt_provider()
		.get_sync_label(ChainMetadata::Genesis)
		.await?
		.expect("Genesis label should be set after sync");
	assert_eq!(
		genesis,
		SyncCheckpoint::new(0, client.api().genesis_hash()),
		"Stored genesis should match chain genesis"
	);

	// Head should be exactly the finalized block at sync start.
	let sync_head = client
		.receipt_provider()
		.get_sync_label(SyncLabel::Head)
		.await?
		.expect("Head should be set after sync");
	assert_eq!(
		sync_head.block_number, finalized_before_sync,
		"Head should equal finalized at sync start"
	);

	// Tail should be genesis (block 0) — on the dev node all blocks have EVM hashes.
	let sync_tail = client
		.receipt_provider()
		.get_sync_label(SyncLabel::Tail)
		.await?
		.expect("Tail should be set after sync");
	assert_eq!(sync_tail, genesis, "Tail should be genesis");

	// On the dev node all blocks (including genesis) have EVM hashes
	let evm_first = client.receipt_provider().get_sync_label(ChainMetadata::FirstEvmBlock).await?;
	assert!(evm_first.is_none(), "FirstEvmBlock should not be set when all blocks are EVM");
	assert_eq!(client.receipt_provider().first_evm_block(), None);

	// Block hash mappings should be queryable after sync.
	let finalized = client.latest_finalized_block().await;
	let substrate_hash = finalized.block_hash();
	let ethereum_hash = client.receipt_provider().get_ethereum_hash(&substrate_hash).await;
	assert!(
		ethereum_hash.is_some(),
		"Finalized block #{} should have an ethereum hash mapping after sync",
		finalized.block_number(),
	);
	assert_eq!(
		client.receipt_provider().get_substrate_hash(&ethereum_hash.unwrap()).await,
		Some(substrate_hash),
		"Reverse mapping should resolve back to the substrate hash"
	);

	// Re-syncing should complete without errors (exercises the resume path).
	client.sync_backward().await?;
	let sync_head_after = client
		.receipt_provider()
		.get_sync_label(SyncLabel::Head)
		.await?
		.expect("Head should exist after re-sync");
	assert!(
		sync_head_after.block_number >= sync_head.block_number,
		"Head should not regress after re-sync"
	);

	Ok(())
}

async fn test_gas_estimation_with_no_funds_and_with_gas_specified() -> anyhow::Result<()> {
	// Arrange
	let code = pallet_revive_fixtures::compile_module_with_type(
		"ContractWithConsumeAllGas",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?
	.0;
	let client = Arc::new(SharedResources::client().await);
	let account = Account::from(Keypair::from_seed([0xFF; 16].as_slice()).unwrap());

	let receipt = TransactionBuilder::new(client.clone())
		.input(code)
		.send()
		.await?
		.wait_for_receipt()
		.await?;
	let contract_address = receipt
		.contract_address
		.expect("Expected the transaction to publish a contract");

	// Act
	let test_function_selector = [0xf8, 0xa8, 0xfd, 0x6d].to_vec();
	let transaction = GenericTransactionV1 {
		from: Some(account.address()),
		input: test_function_selector.into(),
		to: Some(contract_address),
		chain_id: Some(client.chain_id().await?),
		nonce: Some(client.get_transaction_count(account.address(), Default::default()).await?),
		r#type: Some(0u8.into()),
		gas: Some(U256::from(100_000_000u64)),
		..Default::default()
	};
	let dry_run_result = client.estimate_gas(transaction, None).await;

	// Assert
	assert!(matches!(
		dry_run_result, Err(ClientError::Call(error_object))
		if error_object.message().contains("insufficient funds for gas")
	));

	Ok(())
}

/// Simulate an interrupted sync by manually setting both Head and Tail
/// to create a top gap and a bottom gap, then verify that `resume_sync` fills both.
async fn test_block_sync_resume_interrupted() -> anyhow::Result<()> {
	use crate::block_sync::SyncCheckpoint;

	// Submit transactions so the chain has enough blocks for the 1/3 and 2/3 split.
	submit_evm_transfers(6).await?;

	let client = create_sync_test_client().await?;

	// Complete a fresh sync so the DB has all blocks and labels.
	client.sync_backward().await?;

	// Pick two blocks to simulate partial coverage: tail at 1/3, head at 2/3.
	let chain_len = client.latest_finalized_block().await.block_number();

	let tail_num = chain_len / 3;
	let tail_block = client
		.block_provider()
		.block_by_number(tail_num)
		.await?
		.expect("Tail block should exist");

	let head_num = chain_len * 2 / 3;
	let head_block = client
		.block_provider()
		.block_by_number(head_num)
		.await?
		.expect("Head block should exist");

	// Overwrite both labels to simulate an interrupted sync with a partial range.
	let interrupted_tail = SyncCheckpoint::new(tail_block.block_number(), tail_block.block_hash());
	let interrupted_head = SyncCheckpoint::new(head_block.block_number(), head_block.block_hash());

	client
		.receipt_provider()
		.set_sync_label(SyncLabel::Tail, interrupted_tail)
		.await?;
	client
		.receipt_provider()
		.set_sync_label(SyncLabel::Head, interrupted_head)
		.await?;

	// Capture finalized before resume — Head will be set to this snapshot.
	let finalized_before_resume = client.latest_finalized_block().await.block_number();

	// Resume sync — fills top gap and bottom gap.
	client.sync_backward().await?;

	// After resume, Head should be at the finalized block that was current at resume start.
	let new_head = client
		.receipt_provider()
		.get_sync_label(SyncLabel::Head)
		.await?
		.expect("Head should exist after resume");
	assert_eq!(
		new_head.block_number, finalized_before_resume,
		"Head should equal finalized at resume start",
	);

	// Tail should reach genesis (bottom gap fully filled).
	let new_tail = client
		.receipt_provider()
		.get_sync_label(SyncLabel::Tail)
		.await?
		.expect("Tail should exist after resume");
	assert_eq!(
		new_tail.block_number, 0,
		"Tail should be 0 after resume fills the bottom gap, got #{}",
		new_tail.block_number,
	);

	log::debug!(
		target: LOG_TARGET,
		"Resume interrupted OK: simulated partial range #{}..#{}, \
		 after resume tail=#{}, head=#{}",
		interrupted_tail.block_number,
		interrupted_head.block_number,
		new_tail.block_number,
		new_head.block_number,
	);

	Ok(())
}

/// Corrupted sync labels should be detected on resume:
/// - Fake Genesis hash → `ChainMismatch`
/// - Fake Head hash → `SyncBoundaryMismatch`
async fn test_block_sync_detects_corruption() -> anyhow::Result<()> {
	use crate::{block_sync::SyncCheckpoint, client::ClientError};

	// Submit transactions so the chain has enough blocks for the boundary test.
	submit_evm_transfers(2).await?;

	let client = create_sync_test_client().await?;

	// Complete a fresh sync so all labels are stored.
	client.sync_backward().await?;

	// --- ChainMismatch: overwrite Genesis with a fake hash ---
	let fake_genesis = SyncCheckpoint::new(0, H256::from([0xdeu8; 32]));
	client
		.receipt_provider()
		.set_sync_label(ChainMetadata::Genesis, fake_genesis)
		.await?;

	let err = client.sync_backward().await.unwrap_err();
	assert!(matches!(err, ClientError::ChainMismatch), "Expected ChainMismatch, got: {err:?}");

	// Restore the real genesis so we can test the next corruption.
	let real_genesis = SyncCheckpoint::new(0, client.api().genesis_hash());
	client
		.receipt_provider()
		.set_sync_label(ChainMetadata::Genesis, real_genesis)
		.await?;

	// --- SyncBoundaryMismatch: corrupted Head hash ---
	let chain_len = client.latest_finalized_block().await.block_number();
	let corrupted_upper = SyncCheckpoint::new(chain_len / 2, H256::from([0xbau8; 32]));
	client
		.receipt_provider()
		.set_sync_label(SyncLabel::Head, corrupted_upper)
		.await?;

	let err = client.sync_backward().await.unwrap_err();
	assert!(
		matches!(err, ClientError::SyncBoundaryMismatch),
		"Expected SyncBoundaryMismatch, got: {err:?}"
	);

	Ok(())
}

/// Syncing a second client after new transactions have been submitted
/// should include the newer blocks.
async fn test_block_sync_picks_up_new_blocks() -> anyhow::Result<()> {
	// First sync: snapshot the current chain state.
	let client1 = create_sync_test_client().await?;
	let finalized1 = client1.latest_finalized_block().await.block_number();

	client1.sync_backward().await?;

	// Submit a transaction to produce at least one new block.
	submit_evm_transfers(1).await?;

	// Second sync: new client with fresh DB should see the new blocks.
	let client2 = create_sync_test_client().await?;
	let finalized2 = client2.latest_finalized_block().await;

	client2.sync_backward().await?;
	assert!(
		finalized2.block_number() > finalized1,
		"Second finalized #{} should be higher than first #{finalized1}",
		finalized2.block_number(),
	);

	// The new block should have an ethereum hash mapping in client2's DB.
	assert!(
		client2
			.receipt_provider()
			.get_ethereum_hash(&finalized2.block_hash())
			.await
			.is_some(),
		"New finalized block #{} should be synced in client2",
		finalized2.block_number(),
	);

	log::debug!(
		target: LOG_TARGET,
		"Picks up new blocks OK: client2 synced up to #{}, earliest=#{}",
		finalized2.block_number(),
		client2.receipt_provider().first_evm_block().unwrap_or(0),
	);

	Ok(())
}

/// Verifies that overriding the balance of an unfunded EOA allows an `eth_call` that transfers
/// value from it to succeed. Without the override the call would fail due to insufficient funds.
async fn test_state_override_balance() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let sender = AlloyAddress::from([0xAA; 20]);
	let recipient = AlloyAddress::from([0xBB; 20]);

	let tx = TransactionRequest::default()
		.from(sender)
		.to(recipient)
		.value(AlloyU256::from(1_000u64));

	let overrides = StateOverride::from_iter([(
		sender,
		AccountOverride::default().with_balance(AlloyU256::from(1_000_000_000_000_000_000u128)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await;

	// Assert
	assert!(result.is_ok(), "eth_call with balance override should succeed: {result:?}");

	Ok(())
}

/// Verifies that injecting EVM bytecode onto an untouched address (no prior state) via a code
/// override allows calling functions on that address as if it were a deployed contract.
async fn test_state_override_code_empty_to_evm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target = AlloyAddress::from([0xCC; 20]);

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 42 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 42u64,
		"echo(42) should return 42 via EVM code override on empty address"
	);

	Ok(())
}

/// Deploys a contract via the alloy provider and returns the deployed address.
///
/// Uses the provider's configured wallet (alith) to sign and send a deployment transaction,
/// then computes the contract address from the deployer's address and nonce. Constructor
/// arguments, if any, should be ABI-encoded and passed via `constructor_args`.
async fn deploy_contract(
	provider: &(impl Provider + 'static),
	fixture_name: &str,
	fixture_type: pallet_revive_fixtures::FixtureType,
	constructor_args: &[u8],
) -> anyhow::Result<AlloyAddress> {
	let (bytecode, _) =
		pallet_revive_fixtures::compile_module_with_type(fixture_name, fixture_type)?;
	let input: Vec<u8> = bytecode.into_iter().chain(constructor_args.iter().copied()).collect();
	let from = AlloyAddress::from(Account::default().address().0);
	let nonce = provider.get_transaction_count(from).await?;
	let deploy_tx = TransactionRequest::default()
		.from(from)
		.input(AlloyBytes::from(input).into())
		.create();
	provider.send_transaction(deploy_tx).await?.get_receipt().await?;
	Ok(from.create(nonce))
}

/// Verifies that replacing an existing EVM contract's code with different EVM code via override
/// causes the overridden code to execute instead of the original.
async fn test_state_override_code_evm_to_evm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Solc, &[])
			.await?;

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 99 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 99u64,
		"echo(99) should return 99 via EVM code override on EVM contract"
	);

	Ok(())
}

/// Verifies that injecting PVM bytecode onto an untouched address (no prior state) via a code
/// override allows calling Solidity ABI functions on it.
async fn test_state_override_code_empty_to_pvm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target = AlloyAddress::from([0xC1; 20]);

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?;

	let call_data = Callee::echoCall { _data: 42 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 42u64,
		"echo(42) should return 42 via PVM code override on empty address"
	);

	Ok(())
}

/// Verifies that injecting EVM bytecode onto a funded EOA (has balance, no code) via a code
/// override promotes it to a contract and allows calling functions on it.
async fn test_state_override_code_eoa_to_evm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let eoa = AlloyAddress::from([0xC2; 20]);

	// Fund the EOA to make it a real EOA (not just an empty address)
	let fund_tx = TransactionRequest::default()
		.from(from)
		.to(eoa)
		.value(AlloyU256::from(1_000_000_000_000u128));
	provider.send_transaction(fund_tx).await?.get_receipt().await?;

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 55 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(eoa)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		eoa,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 55u64,
		"echo(55) should return 55 via EVM code override on funded EOA"
	);

	Ok(())
}

/// Verifies that injecting PVM bytecode onto a funded EOA (has balance, no code) via a code
/// override promotes it to a contract and allows calling Solidity ABI functions on it.
async fn test_state_override_code_eoa_to_pvm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let eoa = AlloyAddress::from([0xC3; 20]);

	// Fund the EOA to make it a real EOA (not just an empty address)
	let fund_tx = TransactionRequest::default()
		.from(from)
		.to(eoa)
		.value(AlloyU256::from(1_000_000_000_000u128));
	provider.send_transaction(fund_tx).await?.get_receipt().await?;

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?;

	let call_data = Callee::echoCall { _data: 55 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(eoa)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		eoa,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 55u64,
		"echo(55) should return 55 via PVM code override on funded EOA"
	);

	Ok(())
}

/// Verifies that replacing an existing EVM contract's code with PVM bytecode via override causes
/// the PVM code to execute at that address.
async fn test_state_override_code_evm_to_pvm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Solc, &[])
			.await?;

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::Resolc,
	)?;

	let call_data = Callee::echoCall { _data: 88 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 88u64,
		"echo(88) should return 88 via PVM code override on EVM contract"
	);

	Ok(())
}

/// Verifies that replacing an existing PVM contract's code with EVM bytecode via override
/// causes the EVM code to execute at that address.
async fn test_state_override_code_pvm_to_evm() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Resolc, &[])
			.await?;

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 77 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 77u64,
		"echo(77) should return 77 via EVM code override on PVM contract"
	);

	Ok(())
}

/// Verifies that patching a single storage slot via `stateDiff` changes the value returned by
/// a contract's getter while leaving unrelated storage intact.
async fn test_state_override_storage_state_diff() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let counter_address =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Solc, &[])
			.await?;

	let call_data = Counter::numberCall {}.abi_encode();
	let tx = TransactionRequest::default()
		.from(from)
		.to(counter_address)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		counter_address,
		AccountOverride::default()
			.with_state_diff([(B256::ZERO, B256::left_padding_from(&999u64.to_be_bytes()))]),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Counter::numberCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 999,
		"number() should return the overridden value 999, not the original 3"
	);

	Ok(())
}

/// Verifies that a full storage replacement via `state` writes specified slots and that the
/// getter returns the replaced value.
async fn test_state_override_storage_full_replacement() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let counter_address =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Solc, &[])
			.await?;

	let call_data = Counter::numberCall {}.abi_encode();
	let tx = TransactionRequest::default()
		.from(from)
		.to(counter_address)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		counter_address,
		AccountOverride::default()
			.with_state([(B256::ZERO, B256::left_padding_from(&123u64.to_be_bytes()))]),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Counter::numberCall::abi_decode_returns(&result)?;
	assert_eq!(returned_value, 123, "number() should return 123 from the full storage replacement");

	Ok(())
}

/// Verifies that a full storage replacement via `state` zeroes out slots not included in the
/// override. Deploys `TwoSlots(10, 20)`, then overrides with `state` containing only slot 0.
/// Slot 0 should reflect the override, slot 1 should be zero.
async fn test_state_override_storage_full_clears_unspecified() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);

	let constructor_args = TwoSlots::constructorCall { _first: 10, _second: 20 }.abi_encode();
	let target = deploy_contract(
		&provider,
		"TwoSlots",
		pallet_revive_fixtures::FixtureType::Solc,
		&constructor_args,
	)
	.await?;

	let slot_0 = B256::ZERO;

	// Override with `state` containing only slot 0 = 42.
	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default()
			.with_state([(slot_0, B256::left_padding_from(&42u64.to_be_bytes()))]),
	)]);

	// Act — read first() via eth_call with override
	let first_data = TwoSlots::firstCall {}.abi_encode();
	let first_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(first_data).into());
	let first_result = provider.call(first_tx.clone()).overrides(overrides.clone()).await?;
	let first_value = TwoSlots::firstCall::abi_decode_returns(&first_result)?;

	// Act — read second() via eth_call with same override
	let second_data = TwoSlots::secondCall {}.abi_encode();
	let second_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(second_data).into());
	let second_result = provider.call(second_tx.clone()).overrides(overrides).await?;
	let second_value = TwoSlots::secondCall::abi_decode_returns(&second_result)?;

	// Assert
	assert_eq!(first_value, 42, "first() should return the overridden value 42");
	assert_eq!(
		second_value, 0,
		"second() should be zero because full state replacement cleared unspecified slots"
	);

	Ok(())
}

/// Verifies that `stateDiff` only patches the specified slots and leaves unspecified slots
/// untouched. Deploys `TwoSlots(10, 20)`, then overrides slot 0 via `stateDiff`. Slot 0
/// should reflect the override, slot 1 should retain its original value of 20.
async fn test_state_override_storage_diff_preserves_unspecified() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);

	let constructor_args = TwoSlots::constructorCall { _first: 10, _second: 20 }.abi_encode();
	let target = deploy_contract(
		&provider,
		"TwoSlots",
		pallet_revive_fixtures::FixtureType::Solc,
		&constructor_args,
	)
	.await?;

	let slot_0 = B256::ZERO;

	// Override only slot 0 via stateDiff.
	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default()
			.with_state_diff([(slot_0, B256::left_padding_from(&99u64.to_be_bytes()))]),
	)]);

	// Act — read first() with override
	let first_data = TwoSlots::firstCall {}.abi_encode();
	let first_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(first_data).into());
	let first_result = provider.call(first_tx.clone()).overrides(overrides.clone()).await?;
	let first_value = TwoSlots::firstCall::abi_decode_returns(&first_result)?;

	// Act — read second() with same override
	let second_data = TwoSlots::secondCall {}.abi_encode();
	let second_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(second_data).into());
	let second_result = provider.call(second_tx.clone()).overrides(overrides).await?;
	let second_value = TwoSlots::secondCall::abi_decode_returns(&second_result)?;

	// Assert
	assert_eq!(first_value, 99, "first() should return the overridden value 99");
	assert_eq!(
		second_value, 20,
		"second() should retain its original value 20 since stateDiff didn't touch it"
	);

	Ok(())
}

/// Verifies that multiple storage slots can be overridden in a single `stateDiff`. Deploys
/// `TwoSlots(10, 20)`, then overrides both slots via `stateDiff` to different values.
async fn test_state_override_storage_multiple_slots() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);

	let constructor_args = TwoSlots::constructorCall { _first: 10, _second: 20 }.abi_encode();
	let target = deploy_contract(
		&provider,
		"TwoSlots",
		pallet_revive_fixtures::FixtureType::Solc,
		&constructor_args,
	)
	.await?;

	let slot_0 = B256::ZERO;
	let slot_1 = B256::left_padding_from(&1u64.to_be_bytes());

	// Override both slots in one stateDiff.
	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_state_diff([
			(slot_0, B256::left_padding_from(&111u64.to_be_bytes())),
			(slot_1, B256::left_padding_from(&222u64.to_be_bytes())),
		]),
	)]);

	// Act — read first()
	let first_data = TwoSlots::firstCall {}.abi_encode();
	let first_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(first_data).into());
	let first_result = provider.call(first_tx.clone()).overrides(overrides.clone()).await?;
	let first_value = TwoSlots::firstCall::abi_decode_returns(&first_result)?;

	// Act — read second()
	let second_data = TwoSlots::secondCall {}.abi_encode();
	let second_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(second_data).into());
	let second_result = provider.call(second_tx.clone()).overrides(overrides).await?;
	let second_value = TwoSlots::secondCall::abi_decode_returns(&second_result)?;

	// Assert
	assert_eq!(first_value, 111, "first() should return the overridden value 111");
	assert_eq!(second_value, 222, "second() should return the overridden value 222");

	Ok(())
}

/// Verifies that passing `state: {}` (an empty map) clears all storage. Deploys
/// `TwoSlots(10, 20)`, overrides with an empty `state` map, and asserts both slots are zero.
async fn test_state_override_storage_full_empty_map_clears_all() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);

	let constructor_args = TwoSlots::constructorCall { _first: 10, _second: 20 }.abi_encode();
	let target = deploy_contract(
		&provider,
		"TwoSlots",
		pallet_revive_fixtures::FixtureType::Solc,
		&constructor_args,
	)
	.await?;

	// Override with empty `state` map — should clear everything.
	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_state(std::iter::empty()),
	)]);

	// Act — read first()
	let first_data = TwoSlots::firstCall {}.abi_encode();
	let first_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(first_data).into());
	let first_result = provider.call(first_tx.clone()).overrides(overrides.clone()).await?;
	let first_value = TwoSlots::firstCall::abi_decode_returns(&first_result)?;

	// Act — read second()
	let second_data = TwoSlots::secondCall {}.abi_encode();
	let second_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(second_data).into());
	let second_result = provider.call(second_tx.clone()).overrides(overrides).await?;
	let second_value = TwoSlots::secondCall::abi_decode_returns(&second_result)?;

	// Assert
	assert_eq!(first_value, 0, "first() should be zero after empty state override");
	assert_eq!(second_value, 0, "second() should be zero after empty state override");

	Ok(())
}

/// Verifies that setting a storage slot to zero via `stateDiff` results in the getter returning
/// zero. Deploys `TwoSlots(10, 20)`, overrides slot 0 to zero, and asserts `first()` returns 0
/// while `second()` retains its original value.
async fn test_state_override_storage_diff_zero_value() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);

	let constructor_args = TwoSlots::constructorCall { _first: 10, _second: 20 }.abi_encode();
	let target = deploy_contract(
		&provider,
		"TwoSlots",
		pallet_revive_fixtures::FixtureType::Solc,
		&constructor_args,
	)
	.await?;

	// Override slot 0 to zero via stateDiff.
	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_state_diff([(B256::ZERO, B256::ZERO)]),
	)]);

	// Act — read first()
	let first_data = TwoSlots::firstCall {}.abi_encode();
	let first_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(first_data).into());
	let first_result = provider.call(first_tx.clone()).overrides(overrides.clone()).await?;
	let first_value = TwoSlots::firstCall::abi_decode_returns(&first_result)?;

	// Act — read second()
	let second_data = TwoSlots::secondCall {}.abi_encode();
	let second_tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(second_data).into());
	let second_result = provider.call(second_tx.clone()).overrides(overrides).await?;
	let second_value = TwoSlots::secondCall::abi_decode_returns(&second_result)?;

	// Assert
	assert_eq!(first_value, 0, "first() should return zero after stateDiff to zero");
	assert_eq!(second_value, 20, "second() should retain its original value 20");

	Ok(())
}

/// Verifies that overriding the nonce of the `from` account works correctly. The overridden
/// nonce should be used when the transaction doesn't specify one (auto-filled from state).
/// This also validates that the nonce override survives the `prepare_dry_run` nonce bump.
async fn test_state_override_nonce() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let recipient = AlloyAddress::from([0xBB; 20]);

	// A simple transfer call — we don't care about the result, only that the overridden nonce
	// is accepted and doesn't cause a failure.
	let tx = TransactionRequest::default().from(from).to(recipient).nonce(42);

	let overrides = StateOverride::from_iter([(
		from,
		AccountOverride::default()
			.with_nonce(42)
			.with_balance(AlloyU256::from(1_000_000_000_000_000_000u128)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await;

	// Assert
	assert!(result.is_ok(), "eth_call with matching nonce override should succeed: {result:?}");

	Ok(())
}

/// Verifies that overriding both code and storage on the same account in a single override
/// works correctly. The code override promotes the address to a contract, enabling the
/// subsequent storage override to write slots. This tests the ordering guarantee in
/// `apply_single_account_override` (code before storage).
async fn test_state_override_code_and_storage_combined() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target = AlloyAddress::from([0xC4; 20]);

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"TwoSlots",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	// Override: inject TwoSlots runtime code AND set slot 0 = 77 via stateDiff.
	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default()
			.with_code(AlloyBytes::from(code))
			.with_state_diff([(B256::ZERO, B256::left_padding_from(&77u64.to_be_bytes()))]),
	)]);

	// Act — read first() which reads slot 0
	let first_data = TwoSlots::firstCall {}.abi_encode();
	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(first_data).into());
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = TwoSlots::firstCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 77,
		"first() should return 77 from storage override on code-injected address"
	);

	Ok(())
}

/// Verifies that overriding the balance of the `from` account (the transaction sender) works
/// correctly and is not clobbered by `prepare_dry_run`. A sender with zero on-chain balance
/// but an overridden balance should be able to make a value-transferring call.
async fn test_state_override_balance_on_from_account() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let sender = AlloyAddress::from([0xD1; 20]);
	let recipient = AlloyAddress::from([0xD2; 20]);

	// sender has no on-chain balance — the override provides it.
	let tx = TransactionRequest::default()
		.from(sender)
		.to(recipient)
		.value(AlloyU256::from(500u64));

	let overrides = StateOverride::from_iter([(
		sender,
		AccountOverride::default().with_balance(AlloyU256::from(1_000_000_000_000_000_000u128)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await;

	// Assert
	assert!(
		result.is_ok(),
		"balance override on from account should survive prepare_dry_run: {result:?}"
	);

	Ok(())
}

/// Verifies that overriding state of two different accounts in a single `eth_call` works.
/// Both a sender (needing balance) and a target (needing code) are overridden simultaneously.
async fn test_state_override_multiple_accounts() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let sender = AlloyAddress::from([0xDD; 20]);
	let target = AlloyAddress::from([0xEE; 20]);

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 7 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(sender)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([
		(
			sender,
			AccountOverride::default().with_balance(AlloyU256::from(1_000_000_000_000_000_000u128)),
		),
		(target, AccountOverride::default().with_code(AlloyBytes::from(code))),
	]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 7u64,
		"echo(7) should work with sender balance + target code overridden"
	);

	Ok(())
}

/// Verifies that applying both a balance and code override to the same account works.
async fn test_state_override_combined_balance_and_code() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target = AlloyAddress::from([0xFF; 20]);

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 7 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default()
			.with_balance(AlloyU256::from(1_000_000u64))
			.with_code(AlloyBytes::from(code)),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await?;

	// Assert
	let returned_value = Callee::echoCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 7u64,
		"echo(7) should work with combined balance + code override on same account"
	);

	Ok(())
}

/// Verifies that state overrides are ephemeral: a call with overrides sees the overridden state,
/// but a subsequent call without overrides sees the original on-chain state.
async fn test_state_override_does_not_persist() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let counter_address =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Solc, &[])
			.await?;

	let call_data = Counter::numberCall {}.abi_encode();
	let tx = TransactionRequest::default()
		.from(from)
		.to(counter_address)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		counter_address,
		AccountOverride::default()
			.with_state_diff([(B256::ZERO, B256::left_padding_from(&999u64.to_be_bytes()))]),
	)]);

	// Act — call with override
	let overridden_result = provider.call(tx.clone()).overrides(overrides).await?;
	let overridden_value = Counter::numberCall::abi_decode_returns(&overridden_result)?;

	// Act — call without override
	let normal_result = provider.call(tx.clone()).await?;
	let normal_value = Counter::numberCall::abi_decode_returns(&normal_result)?;

	// Assert
	assert_eq!(overridden_value, 999, "overridden call should return 999");
	assert_eq!(normal_value, 3, "subsequent call without override should return original value 3");

	Ok(())
}

/// Verifies that an empty state override set behaves identically to passing no overrides.
async fn test_state_override_empty_set() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let counter_address =
		deploy_contract(&provider, "Counter", pallet_revive_fixtures::FixtureType::Solc, &[])
			.await?;

	let call_data = Counter::numberCall {}.abi_encode();
	let tx = TransactionRequest::default()
		.from(from)
		.to(counter_address)
		.input(AlloyBytes::from(call_data).into());

	let empty_overrides = StateOverride::default();

	// Act
	let result = provider.call(tx.clone()).overrides(empty_overrides).await?;

	// Assert
	let returned_value = Counter::numberCall::abi_decode_returns(&result)?;
	assert_eq!(
		returned_value, 3,
		"empty override set should return the original constructor value 3"
	);

	Ok(())
}

/// Verifies that attempting to override storage on an EOA (without a code override) fails,
/// since EOAs have no contract storage trie.
async fn test_state_override_storage_on_eoa_fails() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let eoa = AlloyAddress::from([0x11; 20]);

	let from = AlloyAddress::from(Account::default().address().0);
	let tx = TransactionRequest::default().from(from).to(eoa);

	let overrides = StateOverride::from_iter([(
		eoa,
		AccountOverride::default()
			.with_state_diff([(B256::ZERO, B256::left_padding_from(&42u64.to_be_bytes()))]),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await;

	// Assert
	assert!(result.is_err(), "storage override on EOA without code should fail: {result:?}");

	Ok(())
}

/// Verifies that overriding balance to zero on a funded account prevents value transfers.
async fn test_state_override_balance_zero() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let ethan = Account::from(subxt_signer::eth::dev::ethan());
	let ethan_alloy = AlloyAddress::from(ethan.address().0);
	let recipient = AlloyAddress::from([0xBB; 20]);

	let tx = TransactionRequest::default()
		.from(ethan_alloy)
		.to(recipient)
		.value(AlloyU256::from(1u64));

	let overrides = StateOverride::from_iter([(
		ethan_alloy,
		AccountOverride::default().with_balance(AlloyU256::ZERO),
	)]);

	// Act
	let result = provider.call(tx.clone()).overrides(overrides).await;

	// Assert
	assert!(
		result.is_err(),
		"call transferring value with balance overridden to zero should fail: {result:?}"
	);

	Ok(())
}

/// Verifies that `debug_traceCall` works with state overrides by injecting code onto an empty
/// address and tracing a call to it. Uses alloy's `DebugApi` to ensure wire-format compatibility
/// with the wider Ethereum ecosystem.
async fn test_state_override_trace_call() -> anyhow::Result<()> {
	// Arrange
	let provider = SharedResources::provider();
	let from = AlloyAddress::from(Account::default().address().0);
	let target = AlloyAddress::from([0xE1; 20]);

	let (code, _) = pallet_revive_fixtures::compile_module_with_type(
		"Callee",
		pallet_revive_fixtures::FixtureType::SolcRuntime,
	)?;

	let call_data = Callee::echoCall { _data: 42 }.abi_encode();

	let tx = TransactionRequest::default()
		.from(from)
		.to(target)
		.input(AlloyBytes::from(call_data).into());

	let overrides = StateOverride::from_iter([(
		target,
		AccountOverride::default().with_code(AlloyBytes::from(code)),
	)]);

	let trace_options = alloy_rpc_types::trace::geth::GethDebugTracingCallOptions {
		tracing_options: alloy_rpc_types::trace::geth::GethDebugTracingOptions {
			tracer: Some(alloy_rpc_types::trace::geth::GethDebugTracerType::BuiltInTracer(
				alloy_rpc_types::trace::geth::GethDebugBuiltInTracerType::CallTracer,
			)),
			..Default::default()
		},
		state_overrides: Some(overrides),
		..Default::default()
	};

	// Act
	let result = provider
		.debug_trace_call_callframe(tx, alloy_rpc_types::BlockId::latest(), trace_options)
		.await;

	// Assert
	assert!(result.is_ok(), "debug_traceCall with state overrides should succeed: {result:?}");
	let frame = result.unwrap();
	assert!(frame.output.is_some(), "trace should have output from echo(42)");

	Ok(())
}

/// Verify that the subscription gap queue backfills blocks for a manually queued range.
async fn test_subscription_gap_filler_backfills_queued_range() -> anyhow::Result<()> {
	submit_evm_transfers(1).await?;

	let (sync_client, gap_fill_rx) = create_sync_test_client_with_subscription_gap_queue().await?;
	sync_client.sync_backward().await?;

	let head_after_sync = sync_client
		.receipt_provider()
		.get_sync_label(SyncLabel::Head)
		.await?
		.expect("Head should be set after sync")
		.block_number;

	// Produce new blocks on-chain; the in-memory test DB has no record of them.
	submit_evm_transfers(3).await?;

	// Query the chain directly; the sync_client's cached finalized block is stale
	// because this client doesn't run subscriptions.
	let new_finalized_number = sync_client.api().at_current_block().await?.block_number();
	assert!(
		new_finalized_number > head_after_sync,
		"New finalized #{new_finalized_number} should be higher than synced head #{head_after_sync}"
	);

	let gap_block = head_after_sync + 1;
	let unsynced_block = sync_client
		.block_provider()
		.block_by_number(gap_block)
		.await?
		.expect("Block should exist on chain");

	// The unsynced block should NOT have a hash mapping yet.
	assert!(
		sync_client
			.receipt_provider()
			.get_ethereum_hash(&unsynced_block.block_hash())
			.await
			.is_none(),
		"Block #{gap_block} should not be in DB before gap fill"
	);

	// Spawn the subscription gap filler, then queue the fill request.
	let bg_client = sync_client.clone();
	let subscription_gap_queue_handle =
		tokio::spawn(async move { bg_client.run_subscription_gap_filler(gap_fill_rx).await });

	assert!(!sync_client.subscription_gap_queue().has_pending());
	sync_client
		.subscription_gap_queue()
		.detect_and_queue(new_finalized_number, head_after_sync);
	assert!(sync_client.subscription_gap_queue().has_pending());

	// Wait for the pending request to be processed.
	let timeout_secs = 10;
	let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
	while sync_client.subscription_gap_queue().has_pending() {
		if tokio::time::Instant::now() > deadline {
			anyhow::bail!("Subscription gap queue did not complete within {timeout_secs}s");
		}
		tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
	}

	// The unsynced block should now have a hash mapping.
	assert!(
		sync_client
			.receipt_provider()
			.get_ethereum_hash(&unsynced_block.block_hash())
			.await
			.is_some(),
		"Block #{gap_block} should be in DB after gap fill"
	);

	// bg_client holds the channel sender, so abort instead of dropping.
	subscription_gap_queue_handle.abort();

	Ok(())
}
