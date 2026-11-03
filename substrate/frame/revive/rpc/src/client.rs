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
//! The client connects to the source substrate chain
//! and is used by the rpc server to query and send transactions to the substrate chain.

pub(crate) mod storage_api;
pub(crate) mod version_aware_runtime_api;

use crate::{
	BlockId, BlockInfoProvider, BlockNumberOrTag, FeeHistoryProvider, FeeHistoryResult, Filter,
	Log, ReceiptInfo, ReceiptProvider, SubxtBlockInfoProvider, SyncLabel, SyncingProgress,
	SyncingStatus, TraceOutcome, TransactionTrace,
	block_sync::SyncCheckpoint,
	subxt_client::{self, SrcChainConfig, revive::calls::EthTransact},
};
use futures::TryStreamExt;
use jsonrpsee::types::{ErrorObjectOwned, error::CALL_EXECUTION_FAILED_CODE};
use pallet_revive::{
	EthTransactError,
	evm::{H256, TransactionSigned, U256, decode_revert_reason},
};
use pallet_revive_types::runtime_api::*;
use sp_runtime::traits::Block as BlockT;
use sp_weights::Weight;
use std::{
	sync::{
		Arc,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	time::Duration,
};
use storage_api::StorageApi;
use subxt::{
	OnlineClient,
	backend::{LegacyBackend, StreamOf, StreamOfResults},
	client::OnlineClientAtBlock,
	config::{HashFor, RpcConfigFor},
	rpcs::{
		RpcClient,
		client::reconnecting_rpc_client::{ExponentialBackoff, RpcClient as ReconnectingRpcClient},
		methods::{
			LegacyRpcMethods,
			legacy::{SystemHealth, TransactionStatus},
		},
		rpc_params,
	},
};

use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use version_aware_runtime_api::{
	CallRecordedOutput, TraceEntry, VersionAwareRuntimeApi, VersionAwareRuntimeApiProvider,
};

/// The substrate block number type.
pub type SubstrateBlockNumber = u64;

/// The substrate block hash type.
pub type SubstrateBlockHash = HashFor<SrcChainConfig>;

/// A handle to a Substrate block at a specific height.
pub type SubstrateBlock = OnlineClientAtBlock<SrcChainConfig>;

/// The runtime balance type.
pub type Balance = u128;

/// The subscription type used to listen to new blocks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SubscriptionType {
	/// Subscribe to best blocks.
	BestBlocks,
	/// Subscribe to finalized blocks.
	FinalizedBlocks,
}

/// Submit Error reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SubmitError {
	/// Transaction was usurped by another with the same nonce.
	#[error("Transaction was usurped by another with the same nonce")]
	Usurped,
	/// Transaction was dropped from the pool.
	#[error("Transaction was dropped")]
	Dropped,
	/// Transaction is invalid (e.g. bad nonce, signature, etc).
	#[error("Transaction is invalid (e.g. bad nonce, signature, etc)")]
	Invalid,
	/// Transaction stream ended without a terminal status.
	#[error("Transaction stream ended without status")]
	StreamEnded,
	/// Unknown transaction status.
	#[error("Unknown transaction status")]
	Unknown,
}

impl From<TransactionStatus<SubstrateBlockHash>> for SubmitError {
	fn from(status: TransactionStatus<SubstrateBlockHash>) -> Self {
		match status {
			TransactionStatus::Usurped(_) => SubmitError::Usurped,
			TransactionStatus::Dropped => SubmitError::Dropped,
			TransactionStatus::Invalid => SubmitError::Invalid,
			_ => SubmitError::Unknown,
		}
	}
}

/// The error type for the client.
#[derive(Error, Debug)]
pub enum ClientError {
	/// A [`jsonrpsee::core::ClientError`] wrapper error.
	#[error(transparent)]
	Jsonrpsee(#[from] jsonrpsee::core::ClientError),
	/// A [`subxt::Error`] wrapper error.
	#[error(transparent)]
	SubxtError(#[from] subxt::Error),
	#[error(transparent)]
	RpcError(#[from] subxt::rpcs::Error),
	/// A [`sqlx::Error`] wrapper error.
	#[error(transparent)]
	SqlxError(#[from] sqlx::Error),
	/// A [`codec::Error`] wrapper error.
	#[error(transparent)]
	CodecError(#[from] codec::Error),
	/// author_submitExtrinsic failed.
	#[error("Invalid transaction: {0}")]
	SubmitError(SubmitError),
	/// Transact call failed.
	#[error("contract reverted: {0:?}")]
	TransactError(EthTransactError),
	/// A decimal conversion failed.
	#[error("conversion failed")]
	ConversionFailed,
	/// The block hash was not found.
	#[error("hash not found")]
	BlockNotFound,
	/// The contract was not found.
	#[error("Contract not found")]
	ContractNotFound,
	#[error("No Ethereum extrinsic found")]
	EthExtrinsicNotFound,
	/// The transaction exists but the node could not produce its trace.
	#[error("trace unavailable: the node could not produce a trace for this transaction")]
	TraceUnavailable,
	/// The transaction fee could not be found
	#[error("transactionFeePaid event not found")]
	TxFeeNotFound,
	/// Failed to decode a raw payload into a signed transaction.
	#[error("Failed to decode a raw payload into a signed transaction")]
	TxDecodingFailed,
	/// Failed to recover eth address.
	#[error("failed to recover eth address")]
	RecoverEthAddressFailed,
	/// Failed to filter logs.
	#[error("Failed to filter logs")]
	LogFilterFailed(#[from] anyhow::Error),
	/// Receipt storage was not found.
	#[error("Receipt storage not found")]
	ReceiptDataNotFound,
	/// Ethereum block was not found.
	#[error("Ethereum block not found")]
	EthereumBlockNotFound,
	/// Receipt data length mismatch.
	#[error("Receipt data length mismatch")]
	ReceiptDataLengthMismatch,
	/// Transaction submission timeout.
	#[error("Transaction submission timeout")]
	Timeout,
	/// Chain identity mismatch between stored genesis and connected node.
	#[error("Genesis hash mismatch")]
	ChainMismatch,
	/// Stored sync boundary does not match the connected node.
	#[error("Sync boundary mismatch")]
	SyncBoundaryMismatch,
	/// The runtime of the queried block does not support the requested runtime API method.
	#[error("The `{0}` runtime API method is not supported at the queried block")]
	UnsupportedRuntimeApiMethod(&'static str),
}

impl ClientError {
	/// Errors that indicate a mismatch between the stored sync state and the connected node.
	pub(crate) fn is_chain_validation_error(&self) -> bool {
		matches!(self, Self::ChainMismatch | Self::SyncBoundaryMismatch)
	}

	/// Why the node cannot service `state_callRecorded`, if it can't; `None` for any genuine
	/// failure that must propagate rather than fall back to a recorder-less replay.
	pub(crate) fn recorded_unavailable_reason(&self) -> Option<RecordedUnavailable> {
		use sc_rpc_api::state::error::{
			CALL_RECORDED_DENIED_ERROR_CODE, CALL_RECORDED_UNSUPPORTED_ERROR_CODE,
		};
		const METHOD_NOT_FOUND: i32 = -32601;

		let ClientError::RpcError(subxt::rpcs::Error::User(e)) = self else {
			return None;
		};
		match e.code {
			METHOD_NOT_FOUND => Some(RecordedUnavailable::MethodMissing),
			CALL_RECORDED_DENIED_ERROR_CODE => Some(RecordedUnavailable::Denied),
			CALL_RECORDED_UNSUPPORTED_ERROR_CODE => Some(RecordedUnavailable::NoRecorder),
			_ => None,
		}
	}
}

/// Why a node cannot service `state_callRecorded`; the variants differ in fallback log detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordedUnavailable {
	/// The node's binary predates `state_callRecorded` (`-32601`).
	MethodMissing,
	/// `state_callRecorded` is unsafe and disabled on this node (`CALL_RECORDED_DENIED`).
	Denied,
	/// The node registers no proof-size recorder (e.g. a solochain).
	NoRecorder,
}

impl RecordedUnavailable {
	/// Whether a recorder-less replay under this reason may have dropped traces.
	pub(crate) fn is_degraded(self) -> bool {
		matches!(self, Self::MethodMissing | Self::Denied)
	}
}

// Direct `From` impls so `?` can lift sub-error variants without an explicit `subxt::Error::from`.
macro_rules! impl_from_subxt_subtype {
	($($ty:ty),* $(,)?) => {
		$(
			impl From<$ty> for ClientError {
				fn from(err: $ty) -> Self {
					ClientError::SubxtError(err.into())
				}
			}
		)*
	};
}

impl_from_subxt_subtype!(
	subxt::error::OnlineClientAtBlockError,
	subxt::error::OnlineClientError,
	subxt::error::BackendError,
	subxt::error::BlockError,
	subxt::error::BlocksError,
	subxt::error::RuntimeApiError,
	subxt::error::EventsError,
	subxt::error::ExtrinsicError,
	subxt::error::ConstantError,
	subxt::error::StorageError,
	subxt::error::StorageValueError,
);

const LOG_TARGET: &str = "eth-rpc::client";
const LOG_TARGET_SUBSCRIPTION: &str = "eth-rpc::subscription";

const REVERT_CODE: i32 = 3;

const NOTIFIER_CAPACITY: usize = 16;

impl From<ClientError> for ErrorObjectOwned {
	fn from(err: ClientError) -> Self {
		match err {
			ClientError::SubxtError(subxt::Error::BackendError(
				subxt::error::BackendError::Rpc(subxt::error::RpcError::ClientError(
					subxt::rpcs::Error::User(err),
				)),
			)) |
			ClientError::RpcError(subxt::rpcs::Error::User(err)) => {
				ErrorObjectOwned::owned::<Vec<u8>>(err.code, err.message, None)
			},
			ClientError::TransactError(EthTransactError::Data(data)) => {
				let msg = match decode_revert_reason(&data) {
					Some(reason) => format!("execution reverted: {reason}"),
					None => "execution reverted".to_string(),
				};

				let data = format!("0x{}", hex::encode(data));
				ErrorObjectOwned::owned::<String>(REVERT_CODE, msg, Some(data))
			},
			ClientError::TransactError(EthTransactError::Message(msg)) => {
				ErrorObjectOwned::owned::<String>(CALL_EXECUTION_FAILED_CODE, msg, None)
			},
			_ => {
				ErrorObjectOwned::owned::<String>(CALL_EXECUTION_FAILED_CODE, err.to_string(), None)
			},
		}
	}
}

/// A client that connects to a substrate node and provides Ethereum-compatible RPC functionality.
#[derive(Clone)]
pub struct Client {
	api: OnlineClient<SrcChainConfig>,
	rpc_client: RpcClient,
	rpc: LegacyRpcMethods<RpcConfigFor<SrcChainConfig>>,
	receipt_provider: ReceiptProvider,
	block_provider: SubxtBlockInfoProvider,
	fee_history_provider: FeeHistoryProvider,
	chain_id: u64,
	max_block_weight: Weight,
	/// Whether the node has automine enabled.
	automine: bool,
	/// A notifier, that informs subscribers of new best blocks.
	block_notifier: Option<tokio::sync::broadcast::Sender<H256>>,
	/// A lock to ensure only one subscription can perform write operations at a time.
	subscription_lock: Arc<Mutex<()>>,

	/// Block subscription sender side.
	block_subscription_tx: tokio::sync::broadcast::Sender<BlockV1>,
	/// Log subscription sender side.
	log_subscription_tx: tokio::sync::broadcast::Sender<Log>,
	/// Whether archive mode is enabled
	is_archive: bool,
	/// Whether historic backfill has completed. `false` if not started or in progress.
	backfill_complete: Arc<AtomicBool>,
	/// Queue for backfilling blocks missed during subscription reconnects.
	subscription_gap_queue: SubscriptionGapQueue,
	/// Hands out the version-aware runtime API of specific blocks.
	runtime_api_provider: VersionAwareRuntimeApiProvider,
}

/// A request to backfill a range of missed blocks (both bounds inclusive).
pub(crate) struct GapFillRequest {
	pub from_inclusive: SubstrateBlockNumber,
	pub to_inclusive: SubstrateBlockNumber,
}

/// Queues gap-fill requests for blocks missed during subscription reconnects.
#[derive(Clone)]
pub(crate) struct SubscriptionGapQueue {
	/// Sender half of the gap-fill queue.
	tx: mpsc::Sender<GapFillRequest>,
	/// Queued + in-flight gap fills. Channel length alone is insufficient
	/// because it drops to zero as soon as the receiver dequeues the item.
	pending: Arc<AtomicUsize>,
}

impl SubscriptionGapQueue {
	pub(crate) fn new() -> (Self, mpsc::Receiver<GapFillRequest>) {
		// Each reconnect produces one gap-fill request for the entire missed range,
		// so 32 allows for 32 rapid disconnects before the consumer processes any.
		let (tx, rx) = mpsc::channel(32);
		(Self { tx, pending: Arc::new(AtomicUsize::new(0)) }, rx)
	}

	/// If `current` is not consecutive to `last`, queue a gap-fill for the missing range.
	pub fn detect_and_queue(&self, current: SubstrateBlockNumber, last: SubstrateBlockNumber) {
		if current.saturating_sub(last) <= 1 {
			return;
		}

		let from_inclusive = current.saturating_sub(1);
		let to_inclusive = last.saturating_add(1);
		let gap_len = from_inclusive.saturating_sub(to_inclusive) + 1;
		self.pending.fetch_add(1, Ordering::Release);
		match self.tx.try_send(GapFillRequest { from_inclusive, to_inclusive }) {
			Ok(_) => {
				log::info!(target: LOG_TARGET,
					"🔄 Subscription gap queue: queued #{from_inclusive} down to #{to_inclusive} ({gap_len} blocks)");
			},
			Err(err) => {
				self.pending.fetch_sub(1, Ordering::Release);
				log::warn!(target: LOG_TARGET,
					"🔄 Subscription gap queue error, dropping #{from_inclusive}..#{to_inclusive} ({gap_len} blocks): {err}");
			},
		}
	}

	/// Mark one request as processed.
	pub fn mark_done(&self) {
		let res = self
			.pending
			.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| v.checked_sub(1));
		if res.is_err() {
			debug_assert!(false, "subscription gap queue pending counter underflowed");
			log::error!(target: LOG_TARGET,
				"🔄 Subscription gap queue pending counter underflow, delete the database and restart with --eth-pruning=archive to resync");
		}
	}

	/// Returns `true` if there are pending gap-fill requests.
	pub fn has_pending(&self) -> bool {
		self.pending.load(Ordering::Acquire) > 0
	}
}

/// Returns the first EVM block number for main and test nets, `None` otherwise.
fn known_first_evm_block_for_chain(chain_id: u64) -> Option<u64> {
	match chain_id {
		420420417 => Some(4_367_914),  // Paseo Asset Hub
		420420418 => Some(12_234_156), // Kusama Asset Hub
		420420419 => Some(11_405_259), // Polkadot Asset Hub
		420420421 => Some(13_169_391), // Westend Asset Hub
		_ => None,
	}
}

/// Fetch the chain ID from the substrate chain.
async fn chain_id(api: &OnlineClient<SrcChainConfig>) -> Result<u64, ClientError> {
	let query = subxt_client::constants().revive().chain_id().unvalidated();
	let at_block = api.at_current_block().await?;
	at_block.constants().entry(query).map_err(|err| err.into())
}

/// Fetch the max block weight from the substrate chain.
async fn max_block_weight(api: &OnlineClient<SrcChainConfig>) -> Result<Weight, ClientError> {
	let query = subxt_client::constants().system().block_weights().unvalidated();
	let at_block = api.at_current_block().await?;
	let weights = at_block.constants().entry(query)?;
	let max_block = weights.per_class.normal.max_extrinsic.unwrap_or(weights.max_block);
	Ok(max_block.0)
}

/// Get the automine status from the node.
async fn get_automine(rpc_client: &RpcClient) -> bool {
	match rpc_client.request::<bool>("getAutomine", rpc_params![]).await {
		Ok(val) => val,
		Err(err) => {
			log::info!(target: LOG_TARGET, "Node does not have getAutomine RPC. Defaulting to automine=false. error: {err:?}");
			false
		},
	}
}

/// Connect to a node at the given URL, and return the underlying API, RPC client, and legacy RPC
/// clients.
pub async fn connect(
	node_rpc_url: &str,
	max_request_size: u32,
	max_response_size: u32,
) -> Result<
	(OnlineClient<SrcChainConfig>, RpcClient, LegacyRpcMethods<RpcConfigFor<SrcChainConfig>>),
	ClientError,
> {
	log::info!(target: LOG_TARGET, "🌐 Connecting to node at: {node_rpc_url} ...");
	let rpc_client = ReconnectingRpcClient::builder()
		.retry_policy(ExponentialBackoff::from_millis(100).max_delay(Duration::from_secs(10)))
		.max_request_size(max_request_size)
		.max_response_size(max_response_size)
		.build(node_rpc_url.to_string())
		.await?;
	let rpc_client = RpcClient::new(rpc_client);
	log::info!(target: LOG_TARGET, "🌟 Connected to node at: {node_rpc_url}");

	// Pin the legacy backend explicitly. Since subxt 0.50, from_rpc_client defaults
	// to the CombinedBackend, which routes block streams and header fetches through
	// the chainHead protocol; its follow restarts and pinning limits stall receipt
	// indexing under load (blocks become unresolvable once unpinned).
	let backend = Arc::new(LegacyBackend::builder().build(rpc_client.clone()));
	let api = OnlineClient::<SrcChainConfig>::from_backend(backend).await?;
	let rpc = LegacyRpcMethods::<RpcConfigFor<SrcChainConfig>>::new(rpc_client.clone());
	Ok((api, rpc_client, rpc))
}

impl Client {
	/// Create a new client instance.
	pub(crate) async fn new(
		api: OnlineClient<SrcChainConfig>,
		rpc_client: RpcClient,
		rpc: LegacyRpcMethods<RpcConfigFor<SrcChainConfig>>,
		block_provider: SubxtBlockInfoProvider,
		receipt_provider: ReceiptProvider,
		is_archive: bool,
		subscription_gap_queue: SubscriptionGapQueue,
		runtime_api_provider: VersionAwareRuntimeApiProvider,
	) -> Result<Self, ClientError> {
		let (chain_id, max_block_weight, automine) =
			tokio::try_join!(chain_id(&api), max_block_weight(&api), async {
				Ok(get_automine(&rpc_client).await)
			},)?;

		// Compute the very first capabilities so that the provider's cache has an anchor which
		// the subscriptions grow forward and the backfill grows backward.
		let latest_finalized_block = block_provider.latest_finalized_block().await;
		runtime_api_provider.at(latest_finalized_block.block_hash()).await?;

		// Fall back to 0 when the hardcoded value exceeds the current best block (e.g. zombienet
		// reusing a known chain ID) and backward sync is disabled.
		if !is_archive {
			if let Some(known) = known_first_evm_block_for_chain(chain_id) {
				let best = block_provider.latest_block_number().await;
				if known > best {
					log::debug!(
						target: LOG_TARGET,
						"Hardcoded first EVM block {known} exceeds best block {best} \
						 for chain {chain_id}, defaulting to 0"
					);
					receipt_provider.set_first_evm_block(0).await?;
				}
			}
		}

		let client = Self {
			api,
			rpc_client,
			rpc,
			receipt_provider,
			block_provider,
			fee_history_provider: FeeHistoryProvider::default(),
			chain_id,
			max_block_weight,
			automine,
			block_notifier: automine
				.then(|| tokio::sync::broadcast::channel::<H256>(NOTIFIER_CAPACITY).0),
			subscription_lock: Arc::new(Mutex::new(())),
			block_subscription_tx: tokio::sync::broadcast::channel(256).0,
			log_subscription_tx: tokio::sync::broadcast::channel(1000).0,
			is_archive,
			backfill_complete: Arc::new(AtomicBool::new(false)),
			subscription_gap_queue,
			runtime_api_provider,
		};

		Ok(client)
	}

	/// Mark historic backfill as complete.
	pub(crate) fn mark_backfill_complete(&self) {
		self.backfill_complete.store(true, Ordering::Release);
	}

	/// Advance the sync_state head label if safe to do so.
	/// Requires: archive mode, historic backfill complete, and no pending gap fills.
	async fn advance_sync_head(&self, block_number: SubstrateBlockNumber, hash: H256) {
		if !self.is_archive ||
			!self.backfill_complete.load(Ordering::Acquire) ||
			self.subscription_gap_queue.has_pending()
		{
			return;
		}

		if let Err(err) = self
			.receipt_provider
			.advance_sync_label(SyncLabel::Head, SyncCheckpoint::new(block_number, hash))
			.await
		{
			log::warn!(target: LOG_TARGET, "Failed to advance sync head: {err:?}");
		}
	}

	/// Creates a block notifier instance.
	pub fn create_block_notifier(&mut self) {
		self.block_notifier = Some(tokio::sync::broadcast::channel::<H256>(NOTIFIER_CAPACITY).0);
	}

	/// Sets a block notifier
	pub fn set_block_notifier(&mut self, notifier: Option<tokio::sync::broadcast::Sender<H256>>) {
		self.block_notifier = notifier;
	}

	pub(crate) fn api(&self) -> &OnlineClient<SrcChainConfig> {
		&self.api
	}

	pub(crate) fn receipt_provider(&self) -> &ReceiptProvider {
		&self.receipt_provider
	}

	pub(crate) fn block_provider(&self) -> &SubxtBlockInfoProvider {
		&self.block_provider
	}

	pub(crate) fn subscription_gap_queue(&self) -> &SubscriptionGapQueue {
		&self.subscription_gap_queue
	}

	/// The earliest block number where the ReviveApi is available.
	/// Resolution order: in-memory value > known-networks table > 0.
	fn earliest_block_number(&self) -> SubstrateBlockNumber {
		self.receipt_provider
			.first_evm_block()
			.or_else(|| known_first_evm_block_for_chain(self.chain_id))
			.unwrap_or(0)
	}

	/// Subscribe to new blocks, and execute the async closure for each block.
	async fn subscribe_new_blocks<F, Fut>(
		&self,
		subscription_type: SubscriptionType,
		callback: F,
	) -> Result<(), ClientError>
	where
		F: Fn(SubstrateBlock) -> Fut + Send + Sync,
		Fut: std::future::Future<Output = Result<(), ClientError>> + Send,
	{
		let mut block_stream = match subscription_type {
			SubscriptionType::BestBlocks => self.api.stream_best_blocks().await,
			SubscriptionType::FinalizedBlocks => self.api.stream_blocks().await,
		}
		.inspect_err(|err| {
			log::error!(target: LOG_TARGET, "Failed to subscribe to blocks: {err:?}");
		})?;

		let mut last_finalized_seen: Option<SubstrateBlockNumber> = None;

		while let Some(block) = block_stream.next().await {
			let block = match block {
				Ok(block) => block,
				Err(err) => {
					let err: subxt::Error = err.into();
					if err.is_disconnected_will_reconnect() {
						log::warn!(
							target: LOG_TARGET,
							"The RPC connection was lost and we may have missed a few blocks \
							({subscription_type:?}, last finalized: {last_finalized_seen:?}): {err:?}"
						);
						continue;
					}

					log::error!(target: LOG_TARGET, "Failed to fetch block ({subscription_type:?}): {err:?}");
					return Err(err.into());
				},
			};

			// Resolution fails for pruned/retracted blocks and on transient RPC errors;
			// erroring out here would kill the essential subscription task and with it the
			// whole server. Skip instead: a skipped finalized block doesn't advance
			// `last_finalized_seen`, so the gap filler backfills it.
			let block = match block.at().await {
				Ok(block) => block,
				Err(err) => {
					log::warn!(
						target: LOG_TARGET,
						"Failed to resolve streamed {subscription_type:?} block #{} ({:?}), skipping: {err:?}",
						block.number(),
						block.hash(),
					);
					continue;
				},
			};

			// Acquire lock to ensure only one subscription can perform write operations at a time
			let _guard = self.subscription_lock.lock().await;

			let block_number = block.block_number();

			// Only check finalized blocks for gaps.
			if subscription_type == SubscriptionType::FinalizedBlocks {
				if let Some(last) = last_finalized_seen {
					self.subscription_gap_queue.detect_and_queue(block_number, last);
				}
				// Update unconditionally — a callback failure doesn't mean the block was missed.
				last_finalized_seen = Some(block_number);
			}

			log::trace!(target: LOG_TARGET_SUBSCRIPTION, "⏳ Processing {subscription_type:?} block: {block_number}");
			if let Err(err) = callback(block).await {
				log::error!(target: LOG_TARGET, "Failed to process block {block_number}: {err:?}");
			} else {
				log::trace!(target: LOG_TARGET_SUBSCRIPTION, "✅ Processed {subscription_type:?} block: {block_number}");
			}
		}

		log::info!(target: LOG_TARGET, "Block subscription ended");
		Ok(())
	}

	/// Extract receipts from a block, persist them and update fee history.
	async fn process_block(
		&self,
		block: &SubstrateBlock,
	) -> Result<(BlockV1, Vec<ReceiptInfo>), ClientError> {
		let block_number = block.block_number();
		let hash = block.block_hash();

		macro_rules! time {
			($label:expr, $expr:expr) => {{
				let t = std::time::Instant::now();
				let r = $expr;
				log::trace!(
					target: LOG_TARGET,
					"⏱️ #{block_number} {}: {:?}",
					$label, t.elapsed(),
				);
				r
			}};
		}

		let eth_block = time!(
			"eth_block",
			self.runtime_api(hash)
				.await?
				.eth_block()
				.ok_or(ClientError::UnsupportedRuntimeApiMethod("eth_block"))?
				.await?
		);
		let receipts = time!(
			"receipts_from_block",
			self.receipt_provider.receipts_from_block(block, eth_block.hash).await?
		);
		time!(
			"insert_block_receipts",
			self.receipt_provider
				.insert_block_receipts(block, &receipts, &eth_block.hash)
				.await?
		);

		let (_, receipt_infos): (Vec<_>, Vec<_>) = receipts.into_iter().unzip();
		self.fee_history_provider.update_fee_history(&eth_block, &receipt_infos).await;

		Ok((eth_block, receipt_infos))
	}

	/// Start the block subscription, and populate the block cache.
	pub async fn subscribe_and_cache_new_blocks(
		&self,
		subscription_type: SubscriptionType,
	) -> Result<(), ClientError> {
		log::info!(target: LOG_TARGET, "🔌 Subscribing to new blocks ({subscription_type:?})");
		self.subscribe_new_blocks(subscription_type, |block| async {
			let hash = block.block_hash();

			match subscription_type {
				SubscriptionType::BestBlocks => {
					let (eth_block, _) = self.process_block(&block).await?;
					self.block_provider.update_latest(Arc::new(block), subscription_type).await;

					if let Some(sender) = &self.block_notifier {
						if sender.receiver_count() > 0 {
							let _ = sender.send(hash);
						}
					}
					if self.block_subscription_tx.receiver_count() > 0 {
						let _ = self.block_subscription_tx.send(eth_block);
					}
				},
				SubscriptionType::FinalizedBlocks => {
					let block_number = block.block_number();
					let (receipt_infos, eth_hash) = match self
						.receipt_provider
						.get_processed_eth_block_hash(block_number, hash)
						.await
					{
						Some(eth_hash) => {
							log::trace!(target: LOG_TARGET_SUBSCRIPTION,
									"⏩ Finalized block #{block_number} already processed, \
									 skipping extraction");
							(None, eth_hash)
						},
						None => {
							let (eth_block, infos) = self.process_block(&block).await?;
							(Some(infos), eth_block.hash)
						},
					};

					self.block_provider.update_latest(Arc::new(block), subscription_type).await;
					self.advance_sync_head(block_number, hash).await;

					if self.log_subscription_tx.receiver_count() > 0 {
						let logs = match receipt_infos {
							Some(infos) => infos.into_iter().flat_map(|r| r.logs).collect(),
							None => {
								self.receipt_provider
									.logs_by_block_number(block_number, eth_hash)
									.await?
							},
						};
						for log in logs {
							let _ = self.log_subscription_tx.send(log);
						}
					}
				},
			}

			Ok(())
		})
		.await
	}

	/// Get the block hash for the given block number or tag.
	pub async fn block_hash_for_tag(&self, at: BlockId) -> Result<SubstrateBlockHash, ClientError> {
		match at {
			BlockId::Hash(hash) => self
				.resolve_substrate_hash(&H256::from(hash.block_hash.0))
				.await
				.ok_or(ClientError::EthereumBlockNotFound),
			BlockId::Number(tag) => self
				.block_by_number_or_tag(&tag)
				.await?
				.map(|block| block.block_hash())
				.ok_or(ClientError::BlockNotFound),
		}
	}

	/// Get a block for the specified hash or number.
	pub async fn block_by_number_or_tag(
		&self,
		block: &BlockNumberOrTag,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		match block {
			BlockNumberOrTag::Number(n) => {
				let n = (*n).try_into().map_err(|_| ClientError::ConversionFailed)?;
				self.block_by_number(n).await
			},
			BlockNumberOrTag::Finalized | BlockNumberOrTag::Safe => {
				let block = self.block_provider.latest_finalized_block().await;
				Ok(Some(block))
			},
			BlockNumberOrTag::Earliest => self.block_by_number(self.earliest_block_number()).await,
			BlockNumberOrTag::Latest | BlockNumberOrTag::Pending => {
				let block = self.block_provider.latest_block().await;
				Ok(Some(block))
			},
		}
	}

	/// Get the block for the given block number or tag, or `None` if it is not known.
	///
	/// Prefer this over resolving a hash with [`Self::block_hash_for_tag`] and then fetching the
	/// block by that hash, which fetches the same block twice.
	pub async fn block_for_tag(
		&self,
		at: BlockId,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		match at {
			BlockId::Hash(hash) => {
				let Some(hash) = self.resolve_substrate_hash(&H256::from(hash.block_hash.0)).await
				else {
					return Ok(None);
				};
				self.block_by_hash(&hash).await
			},
			BlockId::Number(tag) => self.block_by_number_or_tag(&tag).await,
		}
	}

	/// Resolve a [`BlockNumberOrTag`] to a concrete block number.
	async fn resolve_tag_to_number(&self, tag: BlockNumberOrTag) -> Result<U256, ClientError> {
		match tag {
			BlockNumberOrTag::Number(n) => Ok(U256::from(n)),
			BlockNumberOrTag::Earliest => Ok(U256::from(self.earliest_block_number())),
			_ => Ok(self
				.block_by_number_or_tag(&tag)
				.await?
				.ok_or(ClientError::BlockNotFound)?
				.block_number()
				.into()),
		}
	}

	/// Get the storage API for the given block.
	pub async fn storage_api(&self, block_hash: H256) -> Result<StorageApi, ClientError> {
		Ok(StorageApi::new(self.api.at_block(block_hash).await?))
	}

	/// Get the version-aware runtime API for the given block.
	pub async fn runtime_api(
		&self,
		block_hash: H256,
	) -> Result<VersionAwareRuntimeApi, ClientError> {
		self.runtime_api_provider.at(block_hash).await
	}

	/// Get the latest finalized block.
	pub async fn latest_finalized_block(&self) -> Arc<SubstrateBlock> {
		self.block_provider.latest_finalized_block().await
	}

	/// Get the latest best block.
	pub async fn latest_block(&self) -> Arc<SubstrateBlock> {
		self.block_provider.latest_block().await
	}

	/// Submit an ethereum transaction and return a stream of transaction status updates.
	async fn submit_transaction(
		&self,
		call: subxt::tx::StaticPayload<EthTransact>,
	) -> Result<StreamOfResults<TransactionStatus<SubstrateBlockHash>>, ClientError> {
		let at_block = self.api.at_current_block().await?;
		let ext = at_block.tx().create_unsigned(&call.unvalidated()).map_err(ClientError::from)?;

		let sub = self
			.rpc_client
			.subscribe(
				"author_submitAndWatchExtrinsic",
				rpc_params![to_hex(ext.encoded())],
				"author_unwatchExtrinsic",
			)
			.await?;

		let sub = sub.map_err(|e| e.into());
		Ok(StreamOf::new(Box::pin(sub)))
	}

	/// Expose the transaction API.
	pub async fn submit(
		&self,
		call: subxt::tx::StaticPayload<EthTransact>,
	) -> Result<TransactionStatus<SubstrateBlockHash>, ClientError> {
		let mut progress = self.submit_transaction(call).await.inspect_err(|err| {
			log::debug!(target: LOG_TARGET, "Failed to submit transaction: {err:?}");
		})?;

		tokio::time::timeout(Duration::from_secs(5), async {
			if let Some(status) = progress.next().await {
				match status {
					Ok(
						tx @ (TransactionStatus::Future |
						TransactionStatus::Ready |
						// Add other events that follow Ready here for completeness,
						// but they can be ignored.
						TransactionStatus::Broadcast(_) |
						TransactionStatus::InBlock(_) |
						TransactionStatus::FinalityTimeout(_) |
						TransactionStatus::Retracted(_) |
						TransactionStatus::Finalized(_)),
					) => {
						return Ok(tx);
					},
					Ok(
						tx @ (TransactionStatus::Usurped(_) |
						TransactionStatus::Dropped |
						TransactionStatus::Invalid),
					) => {
						return Err(ClientError::SubmitError(tx.into()));
					},
					Err(err) => {
						log::debug!(target: LOG_TARGET, "Transaction submission failed: {err:?}");
						return Err(ClientError::from(err));
					},
				}
			}
			return Err(ClientError::SubmitError(SubmitError::StreamEnded));
		})
		.await
		.map_err(|_| ClientError::Timeout)?
	}

	/// Get an EVM transaction receipt by hash.
	pub async fn receipt(&self, tx_hash: &H256) -> Option<ReceiptInfo> {
		self.receipt_provider.receipt_by_hash(tx_hash).await
	}

	/// Get all transaction receipts for the given block.
	///
	/// Returns `None` if the block does not exist.
	pub async fn block_receipts(
		&self,
		at: BlockId,
	) -> Result<Option<Vec<ReceiptInfo>>, ClientError> {
		let Some(block) = self.block_for_tag(at).await? else {
			return Ok(None);
		};
		let receipts = self.receipt_provider.block_receipts(&block).await?;
		Ok(Some(receipts.into_iter().map(|(_, receipt)| receipt).collect()))
	}

	/// Get The post dispatch weight associated with this Ethereum transaction hash.
	pub async fn post_dispatch_weight(&self, tx_hash: &H256) -> Option<Weight> {
		use crate::subxt_client::system::events::ExtrinsicSuccess;
		let ReceiptInfo { block_hash, transaction_index, .. } = self.receipt(tx_hash).await?;
		let block_hash = self.resolve_substrate_hash(&block_hash).await?;
		let block = self.block_provider.block_by_hash(&block_hash).await.ok()??;
		let extrinsics = block.extrinsics().fetch().await.ok()?;
		let ext = extrinsics.iter().nth(transaction_index.as_u32() as _)?.ok()?;
		let event = ext.events().await.ok()?.find_first::<ExtrinsicSuccess>()?.ok()?;
		Some(event.dispatch_info.weight.0)
	}

	pub async fn sync_state(
		&self,
	) -> Result<sc_rpc::system::SyncState<SubstrateBlockNumber>, ClientError> {
		let client = self.rpc_client.clone();
		let sync_state: sc_rpc::system::SyncState<SubstrateBlockNumber> =
			client.request("system_syncState", Default::default()).await?;
		Ok(sync_state)
	}

	/// Get the syncing status of the chain.
	pub async fn syncing(&self) -> Result<SyncingStatus, ClientError> {
		let health = self.rpc.system_health().await?;

		let status = if health.is_syncing {
			let sync_state = self.sync_state().await?;
			SyncingStatus::SyncingProgress(SyncingProgress {
				current_block: Some(sync_state.current_block.into()),
				highest_block: Some(sync_state.highest_block.into()),
				starting_block: Some(sync_state.starting_block.into()),
			})
		} else {
			SyncingStatus::Bool(false)
		};

		Ok(status)
	}

	/// Get an EVM transaction receipt by hash.
	pub async fn receipt_by_hash_and_index(
		&self,
		block_hash: &H256,
		transaction_index: usize,
	) -> Option<ReceiptInfo> {
		self.receipt_provider
			.receipt_by_block_hash_and_index(block_hash, transaction_index)
			.await
	}

	pub async fn signed_tx_by_hash(&self, tx_hash: &H256) -> Option<TransactionSigned> {
		self.receipt_provider.signed_tx_by_hash(tx_hash).await
	}

	/// Get receipts count per block.
	pub async fn receipts_count_per_block(&self, block_hash: &SubstrateBlockHash) -> Option<usize> {
		self.receipt_provider.receipts_count_per_block(block_hash).await
	}

	/// Get an EVM transaction receipt by specified Ethereum block hash.
	pub async fn receipt_by_ethereum_hash_and_index(
		&self,
		ethereum_hash: &H256,
		transaction_index: usize,
	) -> Option<ReceiptInfo> {
		// Fallback: use hash as Substrate hash if Ethereum hash cannot be resolved
		let substrate_hash =
			self.resolve_substrate_hash(ethereum_hash).await.unwrap_or_else(|| {
				log::trace!(target: LOG_TARGET,
					"receipt_by_ethereum_hash_and_index: no ETH-to-substrate mapping for \
					 {ethereum_hash:?}, falling back to substrate hash lookup");
				*ethereum_hash
			});
		self.receipt_by_hash_and_index(&substrate_hash, transaction_index).await
	}

	/// Get the system health.
	pub async fn system_health(&self) -> Result<SystemHealth, ClientError> {
		let health = self.rpc.system_health().await?;
		Ok(health)
	}

	/// Get the block number of the latest block.
	pub async fn block_number(&self) -> Result<SubstrateBlockNumber, ClientError> {
		let latest_block = self.block_provider.latest_block().await;
		Ok(latest_block.block_number())
	}

	/// Get a block by hash
	pub async fn block_by_hash(
		&self,
		hash: &SubstrateBlockHash,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		self.block_provider.block_by_hash(hash).await
	}

	/// Resolve Ethereum block hash to Substrate block hash, then get the block.
	/// This method provides the abstraction layer needed by the RPC APIs.
	pub async fn resolve_substrate_hash(&self, ethereum_hash: &H256) -> Option<H256> {
		self.receipt_provider.get_substrate_hash(ethereum_hash).await
	}

	/// Resolve Substrate block hash to Ethereum block hash, then get the block.
	/// This method provides the abstraction layer needed by the RPC APIs.
	pub async fn resolve_ethereum_hash(&self, substrate_hash: &H256) -> Option<H256> {
		self.receipt_provider.get_ethereum_hash(substrate_hash).await
	}

	/// Get a block by Ethereum hash with automatic resolution to Substrate hash.
	/// Falls back to treating the hash as a Substrate hash if no mapping exists.
	pub async fn block_by_ethereum_hash(
		&self,
		ethereum_hash: &H256,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		// First try to resolve the Ethereum hash to a Substrate hash
		if let Some(substrate_hash) = self.resolve_substrate_hash(ethereum_hash).await {
			return self.block_by_hash(&substrate_hash).await;
		}

		// Fallback: treat the provided hash as a Substrate hash (backward compatibility)
		log::trace!(target: LOG_TARGET,
			"block_by_ethereum_hash: no ETH-to-substrate mapping for {ethereum_hash:?}, \
			 falling back to substrate hash lookup");
		self.block_by_hash(ethereum_hash).await
	}

	/// Get a block by number
	pub async fn block_by_number(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<Option<Arc<SubstrateBlock>>, ClientError> {
		self.block_provider.block_by_number(block_number).await
	}

	/// Get a block hash for the given block number.
	pub async fn get_block_hash(
		&self,
		block_number: SubstrateBlockNumber,
	) -> Result<Option<SubstrateBlockHash>, ClientError> {
		let maybe_block = self.block_provider.block_by_number(block_number).await?;
		Ok(maybe_block.map(|block| block.block_hash()))
	}

	async fn tracing_block(
		&self,
		block_hash: H256,
	) -> Result<
		sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		ClientError,
	> {
		let signed_block: Option<
			sp_runtime::generic::SignedBlock<
				sp_runtime::generic::Block<
					sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
					sp_runtime::OpaqueExtrinsic,
				>,
			>,
		> = self.rpc_client.request("chain_getBlock", rpc_params![block_hash]).await?;

		Ok(signed_block.ok_or(ClientError::BlockNotFound)?.block)
	}

	/// Get the transaction traces for the given block.
	pub async fn trace_block_by_number(
		&self,
		at: BlockNumberOrTag,
		config: TracerTypeV1,
	) -> Result<Vec<TransactionTrace>, ClientError> {
		if self.receipt_provider.is_before_earliest_block(&at) {
			return Ok(vec![]);
		}

		let block_hash = self.block_hash_for_tag(at.into()).await?;
		let block = self.tracing_block(block_hash).await?;
		let parent_hash = block.header().parent_hash;
		// Block 0 has no parent — there is nothing to trace.
		if parent_hash == Default::default() {
			return Ok(vec![]);
		}

		let CallRecordedOutput { value: trace_entries, degraded } = self
			.runtime_api(parent_hash)
			.await?
			.trace_block(block, config, block_hash)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("trace_block"))?
			.await?;

		let mut hashes = self
			.receipt_provider
			.block_transaction_hashes(&block_hash)
			.await
			.ok_or(ClientError::EthExtrinsicNotFound)?;

		let mut entries = trace_entries
			.into_iter()
			.filter_map(|(index, entry)| {
				let index = index as usize;
				let tx_hash = hashes.remove(&index)?;
				let outcome = match entry {
					TraceEntry::Traced(trace) => TraceOutcome::Trace(trace),
					TraceEntry::NotTraced => {
						TraceOutcome::Error(ClientError::TraceUnavailable.to_string())
					},
				};
				Some((index, TransactionTrace { tx_hash, outcome }))
			})
			.collect::<Vec<_>>();

		if degraded {
			entries.extend(hashes.into_iter().map(|(index, tx_hash)| {
				let outcome = TraceOutcome::Error(ClientError::TraceUnavailable.to_string());
				(index, TransactionTrace { tx_hash, outcome })
			}));
			entries.sort_unstable_by_key(|(index, _)| *index);
		}

		Ok(entries.into_iter().map(|(_, entry)| entry).collect())
	}

	/// Get the transaction traces for the given transaction.
	pub async fn trace_transaction(
		&self,
		transaction_hash: H256,
		config: TracerTypeV1,
	) -> Result<TraceV1, ClientError> {
		let (block_hash, transaction_index) = self
			.receipt_provider
			.find_transaction(&transaction_hash)
			.await
			.ok_or(ClientError::EthExtrinsicNotFound)?;
		let transaction_index = transaction_index as u32;

		let block = self.tracing_block(block_hash).await?;
		let parent_hash = block.header.parent_hash;
		let CallRecordedOutput { value: entry, degraded } = self
			.runtime_api(parent_hash)
			.await?
			.trace_tx(block, transaction_index, config, block_hash)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("trace_tx"))?
			.await?;

		match entry {
			Some(TraceEntry::Traced(trace)) => Ok(trace),
			Some(TraceEntry::NotTraced) => Err(ClientError::TraceUnavailable),
			None if degraded => Err(ClientError::TraceUnavailable),
			None => Err(ClientError::EthExtrinsicNotFound),
		}
	}

	/// Get the transaction traces for the given block.
	pub async fn trace_call(
		&self,
		transaction: GenericTransactionV1,
		block: BlockId,
		config: TracerTypeV1,
		state_overrides: Option<StateOverrideSetV1>,
	) -> Result<TraceV1, ClientError> {
		let block_hash = self.block_hash_for_tag(block).await?;
		let runtime_api = self.runtime_api(block_hash).await?;
		runtime_api
			.trace_call(transaction, config, state_overrides)
			.ok_or(ClientError::UnsupportedRuntimeApiMethod("trace_call"))?
			.await
	}

	/// Get the EVM block for the given Substrate block.
	pub async fn evm_block(
		&self,
		block: Arc<SubstrateBlock>,
		hydrated_transactions: bool,
	) -> Option<BlockV1> {
		log::trace!(target: LOG_TARGET, "Get Ethereum block for hash {:?}", block.block_hash());

		if self
			.receipt_provider
			.is_before_earliest_block(&BlockNumberOrTag::Number(block.block_number().into()))
		{
			log::trace!(target: LOG_TARGET,
				"Block #{} is before receipt floor, skipping", block.block_number());
			return None;
		}

		// This could potentially fail under below circumstances:
		//  - state has been pruned
		//  - the block author cannot be obtained from the digest logs (highly unlikely)
		//  - the node we are targeting has an outdated revive pallet (or ETH block functionality is
		//    disabled)
		let eth_block_result = async {
			self.runtime_api(block.block_hash())
				.await?
				.eth_block()
				.ok_or(ClientError::UnsupportedRuntimeApiMethod("eth_block"))?
				.await
		}
		.await;
		match eth_block_result {
			Ok(mut eth_block) => {
				log::trace!(target: LOG_TARGET, "Ethereum block from runtime API hash {:?}", eth_block.hash);

				if hydrated_transactions {
					// Hydrate the block.
					let tx_infos = self
						.receipt_provider
						.receipts_from_block(&block, eth_block.hash)
						.await
						.inspect_err(|err| {
							log::trace!(target: LOG_TARGET,
								"Failed to extract receipts for block #{}: {err:?}",
								block.block_number());
						})
						.unwrap_or_default()
						.into_iter()
						.map(|(signed_tx, receipt)| receipt.transaction_info(signed_tx))
						.collect::<Vec<_>>();

					eth_block.transactions = HashesOrTransactionInfosV1::TransactionInfos(tx_infos);
				}

				Some(eth_block)
			},
			Err(err) => {
				log::error!(target: LOG_TARGET, "Failed to get Ethereum block for hash {:?}: {err:?}", block.block_hash());
				None
			},
		}
	}

	/// Get the chain ID.
	pub fn chain_id(&self) -> u64 {
		self.chain_id
	}

	/// Get the Max Block Weight.
	pub fn max_block_weight(&self) -> Weight {
		self.max_block_weight
	}

	/// Get the block notifier, if automine is enabled or Self::create_block_notifier was called.
	pub fn block_notifier(&self) -> Option<tokio::sync::broadcast::Sender<H256>> {
		self.block_notifier.clone()
	}

	/// Get the logs matching the given filter.
	pub async fn logs(&self, filter: Option<Filter>) -> Result<Vec<Log>, ClientError> {
		let logs =
			self.receipt_provider
				.logs(filter, |tag| async move {
					self.resolve_tag_to_number(tag).await.map_err(Into::into)
				})
				.await
				.map_err(ClientError::LogFilterFailed)?;

		Ok(logs)
	}

	pub async fn fee_history(
		&self,
		block_count: u32,
		latest_block: BlockNumberOrTag,
		reward_percentiles: Option<Vec<f64>>,
	) -> Result<FeeHistoryResult, ClientError> {
		let Some(latest_block) = self.block_by_number_or_tag(&latest_block).await? else {
			return Err(ClientError::BlockNotFound);
		};

		self.fee_history_provider
			.fee_history(block_count, latest_block.block_number(), reward_percentiles)
			.await
	}

	/// Check if automine is enabled.
	pub fn is_automine(&self) -> bool {
		self.automine
	}

	/// Get the automine status from the node.
	pub async fn get_automine(&self) -> bool {
		get_automine(&self.rpc_client).await
	}

	/// Gets the block subscription rx side of the channel.
	pub fn get_block_subscription_rx(&self) -> tokio::sync::broadcast::Receiver<BlockV1> {
		self.block_subscription_tx.subscribe()
	}

	/// Gets the log subscription rx side of the channel.
	pub fn get_log_subscription_rx(&self) -> tokio::sync::broadcast::Receiver<Log> {
		self.log_subscription_tx.subscribe()
	}
}

fn to_hex(bytes: impl AsRef<[u8]>) -> String {
	format!("0x{}", hex::encode(bytes.as_ref()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use sc_rpc_api::state::error::{
		CALL_RECORDED_DENIED_ERROR_CODE, CALL_RECORDED_UNSUPPORTED_ERROR_CODE,
	};
	use subxt::rpcs::UserError;

	fn rpc_user_error(code: i32) -> ClientError {
		ClientError::RpcError(subxt::rpcs::Error::User(UserError {
			code,
			message: "..".to_string(),
			data: None,
		}))
	}

	#[test]
	fn missing_method_is_version_skew() {
		assert_eq!(
			rpc_user_error(-32601).recorded_unavailable_reason(),
			Some(RecordedUnavailable::MethodMissing),
		);
	}

	#[test]
	fn unsafe_denied_is_a_fallback_reason() {
		assert_eq!(
			rpc_user_error(CALL_RECORDED_DENIED_ERROR_CODE).recorded_unavailable_reason(),
			Some(RecordedUnavailable::Denied),
		);
	}

	#[test]
	fn no_recorder_code_is_expected() {
		assert_eq!(
			rpc_user_error(CALL_RECORDED_UNSUPPORTED_ERROR_CODE).recorded_unavailable_reason(),
			Some(RecordedUnavailable::NoRecorder),
		);
	}

	#[test]
	fn genuine_errors_do_not_trigger_fallback() {
		assert!(rpc_user_error(-32000).recorded_unavailable_reason().is_none());
		assert!(rpc_user_error(-32602).recorded_unavailable_reason().is_none());
		assert!(ClientError::EthExtrinsicNotFound.recorded_unavailable_reason().is_none());
		assert!(ClientError::BlockNotFound.recorded_unavailable_reason().is_none());
	}
}
