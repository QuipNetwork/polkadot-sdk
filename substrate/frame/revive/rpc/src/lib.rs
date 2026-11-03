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
//! The [`EthRpcServer`] RPC server implementation
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use alloy_rpc_types::{BlockId, BlockNumberOrTag, Filter, FilterBlockOption};
use client::ClientError;
use futures::{Stream, StreamExt, TryStreamExt};
use jsonrpsee::{
	PendingSubscriptionSink, SubscriptionMessage, SubscriptionSink,
	core::{RpcResult, async_trait},
	types::{ErrorCode, ErrorObjectOwned},
};
use pallet_revive::evm::*;
use pallet_revive_types::runtime_api::{
	BlockV1, ExecutionTracerConfigV1, GenericTransactionV1, ReceiptGasInfoV1, StateOverrideSetV1,
	TraceV1, TracerTypeV1, TransactionInfoV1,
};
use sp_core::{H160, H256, U256};
use sp_crypto_hashing::keccak_256;
use std::pin::Pin;
use subxt::rpcs::methods::legacy::TransactionStatus;
use thiserror::Error;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};

mod block_sync;
pub(crate) use block_sync::{ChainMetadata, SyncLabel, SyncStateKey};
pub mod cli;
pub mod client;
pub mod example;
pub mod subxt_client;

#[cfg(test)]
mod tests;

mod block_info_provider;
pub use block_info_provider::*;

mod receipt_provider;
pub use receipt_provider::*;

mod fee_history_provider;
pub use fee_history_provider::*;

mod receipt_extractor;
pub use receipt_extractor::*;

mod apis;
pub use apis::*;

mod types;
pub use types::*;

pub const LOG_TARGET: &str = "eth-rpc";

/// An EVM RPC server implementation.
pub struct EthRpcServerImpl {
	/// The client used to interact with the substrate node.
	client: client::Client,

	/// The accounts managed by the server.
	accounts: Vec<Account>,

	/// Controls if unprotected txs are allowed or not.
	allow_unprotected_txs: bool,

	/// When true, estimate_gas uses Pending block if no block is specified.
	use_pending_for_estimate_gas: bool,
}

impl EthRpcServerImpl {
	/// Creates a new [`EthRpcServerImpl`].
	pub fn new(client: client::Client) -> Self {
		Self {
			client,
			accounts: vec![],
			allow_unprotected_txs: false,
			use_pending_for_estimate_gas: false,
		}
	}

	/// Sets the accounts managed by the server.
	pub fn with_accounts(mut self, accounts: Vec<Account>) -> Self {
		self.accounts = accounts;
		self
	}

	/// Sets whether unprotected transactions are allowed or not.
	pub fn with_allow_unprotected_txs(mut self, allow_unprotected_txs: bool) -> Self {
		self.allow_unprotected_txs = allow_unprotected_txs;
		self
	}

	/// Sets whether estimate_gas uses Pending block when no block is specified.
	pub fn with_use_pending_for_estimate_gas(mut self, use_pending_for_estimate_gas: bool) -> Self {
		self.use_pending_for_estimate_gas = use_pending_for_estimate_gas;
		self
	}
}

/// The error type for the EVM RPC server.
#[derive(Error, Debug)]
pub enum EthRpcError {
	/// A [`ClientError`] wrapper error.
	#[error("Client error: {0}")]
	ClientError(#[from] ClientError),
	/// A [`rlp::DecoderError`] wrapper error.
	#[error("Decoding error: {0}")]
	RlpError(#[from] rlp::DecoderError),
	/// A Decimals conversion error.
	#[error("Conversion error")]
	ConversionError,
	/// An invalid signature error.
	#[error("Invalid signature")]
	InvalidSignature,
	/// The account was not found at the given address
	#[error("Account not found for address {0:?}")]
	AccountNotFound(H160),
	/// Received an invalid transaction
	#[error("Invalid transaction")]
	InvalidTransaction,
	/// Received an invalid transaction
	#[error("Invalid transaction {0:?}")]
	TransactionTypeNotSupported(Byte),
	/// The requested `eth_feeHistory` reward percentiles are invalid.
	#[error("{0}")]
	InvalidRewardPercentiles(String),
}

impl From<EthRpcError> for ErrorObjectOwned {
	fn from(value: EthRpcError) -> Self {
		use jsonrpsee::types::error::CALL_EXECUTION_FAILED_CODE;
		let message = value.to_string();
		let code = match value {
			// `ClientError` already produces a fully formed JSON-RPC error object.
			EthRpcError::ClientError(err) => return Self::from(err),
			EthRpcError::ConversionError => ErrorCode::InvalidParams.code(),
			// Matches Geth/Nethermind, which return `-32000` for these execution-time errors.
			EthRpcError::RlpError(_) |
			EthRpcError::InvalidSignature |
			EthRpcError::AccountNotFound(_) |
			EthRpcError::InvalidTransaction |
			EthRpcError::InvalidRewardPercentiles(_) |
			EthRpcError::TransactionTypeNotSupported(_) => CALL_EXECUTION_FAILED_CODE,
		};
		Self::owned::<String>(code, message, None)
	}
}

#[async_trait]
impl EthRpcServer for EthRpcServerImpl {
	async fn net_version(&self) -> RpcResult<String> {
		Ok(self.client.chain_id().to_string())
	}

	async fn net_listening(&self) -> RpcResult<bool> {
		let syncing = self.client.syncing().await?;
		let listening = matches!(syncing, SyncingStatus::Bool(false));
		Ok(listening)
	}

	async fn syncing(&self) -> RpcResult<SyncingStatus> {
		Ok(self.client.syncing().await?)
	}

	async fn block_number(&self) -> RpcResult<U256> {
		let number = self.client.block_number().await?;
		Ok(number.into())
	}

	async fn get_transaction_receipt(
		&self,
		transaction_hash: H256,
	) -> RpcResult<Option<ReceiptInfo>> {
		let receipt = self.client.receipt(&transaction_hash).await;
		Ok(receipt)
	}

	/// Performs gas estimations to find the lowest gas limit required to run the transaction.
	///
	/// This method implements the same gas estimation logic found in Geth which performs binary
	/// search with some simple heuristics to find the smallest gas limit for the transaction.
	async fn estimate_gas(
		&self,
		transaction: GenericTransactionV1,
		block: Option<BlockNumberOrTag>,
	) -> RpcResult<U256> {
		log::trace!(target: LOG_TARGET, "estimate_gas transaction={transaction:?} block={block:?}");

		let block = block.unwrap_or_else(|| {
			if self.use_pending_for_estimate_gas {
				BlockNumberOrTag::Pending
			} else {
				Default::default()
			}
		});
		let block = BlockId::from(block);
		let hash = self.client.block_hash_for_tag(block).await?;
		let gas_estimate = self
			.client
			.runtime_api(hash)
			.await?
			.estimate_gas(transaction, block)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("eth_estimate_gas"))?
			.await?;

		log::trace!(
			target: LOG_TARGET,
			"estimate_gas result={gas_estimate:?}",
		);
		Ok(gas_estimate)
	}

	async fn call(
		&self,
		transaction: GenericTransactionV1,
		block: Option<BlockId>,
		state_overrides: Option<StateOverrideSetV1>,
	) -> RpcResult<Bytes> {
		let block = block.unwrap_or_default();
		let hash = self.client.block_hash_for_tag(block).await?;
		let runtime_api = self.client.runtime_api(hash).await?;
		let dry_run = runtime_api
			.dry_run(transaction, block, state_overrides)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("eth_transact"))?
			.await?;
		Ok(dry_run.data.into())
	}

	async fn send_raw_transaction(&self, transaction: Bytes) -> RpcResult<H256> {
		let hash = H256(keccak_256(&transaction.0));
		log::trace!(target: LOG_TARGET, "send_raw_transaction transaction: {transaction:?} ethereum_hash: {hash:?}");

		if !self.allow_unprotected_txs {
			let signed_transaction = TransactionSigned::decode(transaction.0.as_slice())
				.map_err(|err| {
					log::trace!(target: LOG_TARGET, "Transaction decoding failed. ethereum_hash: {hash:?}, error: {err:?}");
					EthRpcError::InvalidTransaction
				})?;

			let is_chain_id_provided = match signed_transaction {
				TransactionSigned::Transaction7702Signed(tx) => {
					tx.transaction_7702_unsigned.chain_id != U256::zero()
				},
				TransactionSigned::Transaction4844Signed(tx) => {
					tx.transaction_4844_unsigned.chain_id != U256::zero()
				},
				TransactionSigned::Transaction1559Signed(tx) => {
					tx.transaction_1559_unsigned.chain_id != U256::zero()
				},
				TransactionSigned::Transaction2930Signed(tx) => {
					tx.transaction_2930_unsigned.chain_id != U256::zero()
				},
				TransactionSigned::TransactionLegacySigned(tx) => {
					tx.transaction_legacy_unsigned.chain_id.is_some()
				},
			};

			if !is_chain_id_provided {
				log::trace!(target: LOG_TARGET, "Invalid Transaction: transaction doesn't include a chain-id. ethereum_hash: {hash:?}");
				Err(EthRpcError::InvalidTransaction)?;
			}
		}

		let call = subxt_client::tx().revive().eth_transact(transaction.0);

		// Subscribe to new block only when automine is enabled.
		let receiver = self.client.block_notifier().map(|sender| sender.subscribe());

		// Submit the transaction
		let tx_status = self.client.submit(call).await.map_err(|err| {
			log::trace!(target: LOG_TARGET, "send_raw_transaction ethereum_hash: {hash:?} failed: {err:?}");
			err
		})?;

		if matches!(tx_status, TransactionStatus::Future) {
			return Ok(hash);
		}

		// Wait for the transaction to be included in a block if automine is enabled
		if let Some(mut receiver) = receiver {
			loop {
				if let Ok(block_hash) = receiver.recv().await {
					let Ok(Some(block)) = self.client.block_by_hash(&block_hash).await else {
						log::debug!(target: LOG_TARGET, "Could not find the block with the received hash: {hash:?}.");
						continue;
					};
					let Some(evm_block) = self.client.evm_block(block, false).await else {
						log::debug!(target: LOG_TARGET, "Failed to get the EVM block for substrate block with hash: {hash:?}");
						continue;
					};
					if evm_block.transactions.contains_tx(hash) {
						log::debug!(target: LOG_TARGET, "{hash:} was included in a block");
						break;
					}
				}
			}
		}

		log::debug!(target: LOG_TARGET, "send_raw_transaction hash: {hash:?}");
		Ok(hash)
	}

	async fn send_transaction(&self, mut transaction: GenericTransactionV1) -> RpcResult<H256> {
		log::debug!(target: LOG_TARGET, "{transaction:#?}");

		let Some(from) = transaction.from else {
			log::debug!(target: LOG_TARGET, "Transaction must have a sender");
			return Err(EthRpcError::InvalidTransaction.into());
		};

		let account = self
			.accounts
			.iter()
			.find(|account| account.address() == from)
			.ok_or(EthRpcError::AccountNotFound(from))?;

		if transaction.gas.is_none() {
			transaction.gas = Some(self.estimate_gas(transaction.clone(), None).await?);
		}

		if transaction.gas_price.is_none() {
			transaction.gas_price = Some(self.gas_price().await?);
		}

		if transaction.nonce.is_none() {
			transaction.nonce = Some(self.get_transaction_count(from, Default::default()).await?);
		}

		if transaction.chain_id.is_none() {
			transaction.chain_id = Some(self.chain_id().await?);
		}

		let tx = GenericTransaction::from(transaction)
			.try_into_unsigned()
			.map_err(|_| EthRpcError::InvalidTransaction)?;
		let payload = account.sign_transaction(tx).signed_payload();
		self.send_raw_transaction(Bytes(payload)).await
	}

	async fn get_block_by_hash(
		&self,
		block_hash: H256,
		hydrated_transactions: bool,
	) -> RpcResult<Option<BlockV1>> {
		let Some(block) = self.client.block_by_ethereum_hash(&block_hash).await? else {
			return Ok(None);
		};
		let block = self.client.evm_block(block, hydrated_transactions).await;
		Ok(block)
	}

	async fn get_balance(&self, address: H160, block: BlockId) -> RpcResult<U256> {
		let hash = self.client.block_hash_for_tag(block).await?;
		let runtime_api = self.client.runtime_api(hash).await?;
		let balance = runtime_api
			.balance(address)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("balance"))?
			.await?;
		Ok(balance)
	}

	async fn chain_id(&self) -> RpcResult<U256> {
		Ok(self.client.chain_id().into())
	}

	async fn gas_price(&self) -> RpcResult<U256> {
		let hash = self.client.block_hash_for_tag(Default::default()).await?;
		let runtime_api = self.client.runtime_api(hash).await?;
		let gas_price = runtime_api
			.gas_price()
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("gas_price"))?
			.await?;
		Ok(gas_price)
	}

	async fn max_priority_fee_per_gas(&self) -> RpcResult<U256> {
		// We do not support tips. Hence the recommended priority fee is
		// always zero. The effective gas price will always be the base price.
		Ok(Default::default())
	}

	async fn get_code(&self, address: H160, block: BlockId) -> RpcResult<Bytes> {
		let hash = self.client.block_hash_for_tag(block).await?;
		let code = self
			.client
			.runtime_api(hash)
			.await?
			.code(address)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("code"))?
			.await?;
		Ok(code.into())
	}

	async fn accounts(&self) -> RpcResult<Vec<H160>> {
		Ok(self.accounts.iter().map(|account| account.address()).collect())
	}

	async fn get_block_by_number(
		&self,
		block_number: BlockNumberOrTag,
		hydrated_transactions: bool,
	) -> RpcResult<Option<BlockV1>> {
		let Some(block) = self.client.block_by_number_or_tag(&block_number).await? else {
			return Ok(None);
		};
		let block = self.client.evm_block(block, hydrated_transactions).await;
		Ok(block)
	}

	async fn get_block_receipts(&self, block: BlockId) -> RpcResult<Option<Vec<ReceiptInfo>>> {
		let receipts = self.client.block_receipts(block).await?;
		Ok(receipts)
	}

	async fn get_block_transaction_count_by_hash(
		&self,
		block_hash: Option<H256>,
	) -> RpcResult<Option<U256>> {
		let block_hash = if let Some(block_hash) = block_hash {
			block_hash
		} else {
			self.client.latest_block().await.hash()
		};

		let Some(substrate_hash) = self.client.resolve_substrate_hash(&block_hash).await else {
			return Ok(None);
		};

		Ok(self.client.receipts_count_per_block(&substrate_hash).await.map(U256::from))
	}

	async fn get_block_transaction_count_by_number(
		&self,
		block: Option<BlockNumberOrTag>,
	) -> RpcResult<Option<U256>> {
		let substrate_hash = if let Some(block) = self
			.client
			.block_by_number_or_tag(&block.unwrap_or_else(|| BlockNumberOrTag::Latest))
			.await?
		{
			block.block_hash()
		} else {
			return Ok(None);
		};

		Ok(self.client.receipts_count_per_block(&substrate_hash).await.map(U256::from))
	}

	async fn get_logs(&self, filter: Option<Filter>) -> RpcResult<FilterResults> {
		let logs = self.client.logs(filter).await?;
		Ok(FilterResults::Logs(logs))
	}

	async fn get_storage_at(
		&self,
		address: H160,
		storage_slot: U256,
		block: BlockId,
	) -> RpcResult<Bytes> {
		let hash = self.client.block_hash_for_tag(block).await?;
		let runtime_api = self.client.runtime_api(hash).await?;
		let get_storage = runtime_api
			.get_storage(address, storage_slot.to_big_endian())
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("get_storage"))?;
		let bytes = match get_storage.await {
			Ok(value) => value.unwrap_or([0u8; 32].into()),
			// Per Ethereum spec, return zero for non-contract addresses.
			Err(ClientError::ContractNotFound) => {
				log::trace!(target: LOG_TARGET, "get_storage_at: ContractNotFound for {address:?}, returning zero");
				[0u8; 32].into()
			},
			Err(err) => return Err(err.into()),
		};
		Ok(bytes.into())
	}

	async fn get_transaction_by_block_hash_and_index(
		&self,
		block_hash: H256,
		transaction_index: U256,
	) -> RpcResult<Option<TransactionInfoV1>> {
		let Some(substrate_block_hash) = self.client.resolve_substrate_hash(&block_hash).await
		else {
			return Ok(None);
		};
		self.get_transaction_by_substrate_block_hash_and_index(
			substrate_block_hash,
			transaction_index,
		)
		.await
	}

	async fn get_transaction_by_block_number_and_index(
		&self,
		block: BlockNumberOrTag,
		transaction_index: U256,
	) -> RpcResult<Option<TransactionInfoV1>> {
		let Some(block) = self.client.block_by_number_or_tag(&block).await? else {
			return Ok(None);
		};
		self.get_transaction_by_substrate_block_hash_and_index(
			block.block_hash(),
			transaction_index,
		)
		.await
	}

	async fn get_transaction_by_hash(
		&self,
		transaction_hash: H256,
	) -> RpcResult<Option<TransactionInfoV1>> {
		let receipt = self.client.receipt(&transaction_hash).await;
		let signed_tx = self.client.signed_tx_by_hash(&transaction_hash).await;
		if let (Some(receipt), Some(signed_tx)) = (receipt, signed_tx) {
			return Ok(Some(receipt.transaction_info(signed_tx)));
		}

		Ok(None)
	}

	async fn get_transaction_count(&self, address: H160, block: BlockId) -> RpcResult<U256> {
		let hash = self.client.block_hash_for_tag(block).await?;
		let runtime_api = self.client.runtime_api(hash).await?;
		let nonce = runtime_api
			.nonce(address)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("nonce"))?
			.await?;
		Ok(nonce)
	}

	async fn web3_client_version(&self) -> RpcResult<String> {
		let git_revision = env!("GIT_REVISION");
		let rustc_version = env!("RUSTC_VERSION");
		let target = env!("TARGET");
		Ok(format!("eth-rpc/{git_revision}/{target}/{rustc_version}"))
	}

	async fn fee_history(
		&self,
		block_count: U256,
		newest_block: BlockNumberOrTag,
		reward_percentiles: Option<Vec<f64>>,
	) -> RpcResult<FeeHistoryResult> {
		let block_count: u32 = block_count.try_into().map_err(|_| EthRpcError::ConversionError)?;

		// As in go-ethereum, a request for fewer than one block returns an empty result rather
		// than an error, before any percentile validation.
		if block_count == 0 {
			return Ok(FeeHistoryResult {
				oldest_block: U256::zero(),
				base_fee_per_gas: vec![],
				gas_used_ratio: vec![],
				reward: vec![],
			});
		}

		// Reject malformed percentiles up front, as go-ethereum does, instead of silently
		// clamping or approximating them at the wrong bucket.
		if let Some(percentiles) = reward_percentiles.as_deref() {
			validate_reward_percentiles(percentiles)
				.map_err(EthRpcError::InvalidRewardPercentiles)?;
		}

		let result = self.client.fee_history(block_count, newest_block, reward_percentiles).await?;
		Ok(result)
	}

	async fn eth_subscribe(
		&self,
		pending: PendingSubscriptionSink,
		kind: SubscriptionKind,
		options: Option<SubscriptionOptions>,
	) {
		let Some(subscription_parameters) = SubscriptionParameters::new(kind, options) else {
			return pending
				.reject(ErrorObjectOwned::owned(
					jsonrpsee::types::error::INVALID_PARAMS_CODE,
					"Invalid subscription parameters",
					None::<()>,
				))
				.await;
		};
		let Ok(sink) = pending.accept().await else {
			return;
		};

		let stream: Pin<
			Box<dyn Stream<Item = Result<SubscriptionItem, BroadcastStreamRecvError>> + Send>,
		> = match subscription_parameters {
			SubscriptionParameters::NewBlockHeaders => Box::pin(
				BroadcastStream::new(self.client.get_block_subscription_rx())
					.map_ok(|block| SubscriptionItem::BlockHeader(BlockHeader::from(block))),
			) as _,
			SubscriptionParameters::Logs(filter) => Box::pin(
				BroadcastStream::new(self.client.get_log_subscription_rx())
					.try_filter(move |log| futures::future::ready(filter.matches(log)))
					.map_ok(SubscriptionItem::Log),
			) as _,
		};
		let _ = tokio::spawn(Self::handle_subscription_forwarding(sink, stream));
	}
}

impl EthRpcServerImpl {
	async fn get_transaction_by_substrate_block_hash_and_index(
		&self,
		substrate_block_hash: H256,
		transaction_index: U256,
	) -> RpcResult<Option<TransactionInfoV1>> {
		let Some(receipt) = self
			.client
			.receipt_by_hash_and_index(
				&substrate_block_hash,
				transaction_index.try_into().map_err(|_| EthRpcError::ConversionError)?,
			)
			.await
		else {
			return Ok(None);
		};
		let Some(signed_tx) = self.client.signed_tx_by_hash(&receipt.transaction_hash).await else {
			return Ok(None);
		};

		Ok(Some(receipt.transaction_info(signed_tx)))
	}

	async fn handle_subscription_forwarding(
		sink: SubscriptionSink,
		mut stream: Pin<
			Box<dyn Stream<Item = Result<SubscriptionItem, BroadcastStreamRecvError>> + Send>,
		>,
	) {
		loop {
			tokio::select! {
				_ = sink.closed() => break,
				item = stream.next() => {
					match item {
						// Stream ended.
						None => break,
						// Send the item to the subscriber.
						Some(Ok(sub_item)) => {
							let msg = SubscriptionMessage::from_json(&sub_item)
								.expect("SubscriptionItem is serializable; qed");
							if sink.send(msg).await.is_err() {
								break;
							}
						},
						// Broadcast receiver lagged behind — missed messages.
						Some(Err(BroadcastStreamRecvError::Lagged(count))) => {
							log::warn!(
								target: LOG_TARGET,
								"Subscription lagged, skipped {count} messages"
							);
						},
					}
				}
			}
		}
	}
}

#[cfg(test)]
mod error_codes_tests {
	use super::*;
	use jsonrpsee::types::error::{CALL_EXECUTION_FAILED_CODE, INVALID_PARAMS_CODE};

	#[test]
	fn eth_rpc_error_maps_to_expected_code_and_message() {
		let cases: Vec<(EthRpcError, i32)> = vec![
			(EthRpcError::RlpError(rlp::DecoderError::RlpIsTooShort), CALL_EXECUTION_FAILED_CODE),
			(EthRpcError::ConversionError, INVALID_PARAMS_CODE),
			(EthRpcError::InvalidSignature, CALL_EXECUTION_FAILED_CODE),
			(EthRpcError::AccountNotFound(H160::repeat_byte(0xab)), CALL_EXECUTION_FAILED_CODE),
			(EthRpcError::InvalidTransaction, CALL_EXECUTION_FAILED_CODE),
			(
				EthRpcError::TransactionTypeNotSupported(Byte::from(0x7eu8)),
				CALL_EXECUTION_FAILED_CODE,
			),
		];

		for (err, expected_code) in cases {
			let expected_message = err.to_string();
			let obj = ErrorObjectOwned::from(err);
			assert_eq!(obj.code(), expected_code, "unexpected code for `{expected_message}`");
			assert_eq!(obj.message(), expected_message);
		}
	}
}
