// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A version-aware access layer for pallet-revive's runtime API together with the provider that
//! tracks which runtime API methods are available in each runtime spec version.

use crate::{
	BlockId,
	client::{Balance, ClientError, RecordedUnavailable, SubstrateBlockNumber},
	subxt_client::{self, SrcChainConfig},
};
use futures::{FutureExt, TryFutureExt, future::BoxFuture};
use pallet_revive::evm::{H160, U256};
use pallet_revive_types::runtime_api::*;
use sp_core::{Bytes, H256};
use sp_timestamp::Timestamp;
use std::{
	collections::HashMap,
	future::Future,
	sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use subxt::{
	Metadata, OnlineClient,
	client::OnlineClientAtBlock,
	error::RuntimeApiError,
	ext::{frame_decode, scale_decode::IntoVisitor},
	rpcs::{RpcClient, rpc_params},
	runtime_apis::{Payload, StaticPayload},
};

use MethodStatus::*;
use MethodVersioningStatus::*;

const LOG_TARGET: &str = "eth-rpc::version-aware-runtime-api";

/// A version-aware runtime API wrapper for pallet-revive.
///
/// This is an abstraction for the runtime API of pallet-revive which allows us to call the methods
/// appropriate for each version without this leaking into the rest of the codebase.
///
/// All of the futures returned by this layer are [`Option<BoxFuture>`]. This is an intentional
/// decision so that versions which do not support the required runtime API functions return no
/// future which needs to be awaited nor resolved. For example, calling a runtime API function on
/// a version of asset-hub which doesn't have pallet-revive should yield [`None`] rather than us
/// attempting to call the method only for us to find that it's not available.
///
/// This layer doesn't have a 1:1 mapping of abstraction method to runtime API method. Rather, the
/// methods on this layer are grouped conceptually. For example, the [`estimate_gas`] method could
/// call any of the runtime API functions `estimate_gas`, `eth_transact_with_config`, or
/// `eth_transact` depending on their availability, which we do in this layer since it's the layer
/// that holds the information and the knowledge of what runtime API functions are available and
/// what isn't. Therefore it's not a 1:1 mapping.
///
/// [`estimate_gas`]: VersionAwareRuntimeApi::estimate_gas
pub struct VersionAwareRuntimeApi {
	at_block: OnlineClientAtBlock<SrcChainConfig>,
	capabilities: ReviveRuntimeApiCapabilities,
	rpc_client: RpcClient,
}

/// The decoded runtime API value, plus whether it came from a recorder-less fallback replay that
/// may have dropped traces.
pub struct CallRecordedOutput<T> {
	pub value: T,
	pub degraded: bool,
}

impl<T> CallRecordedOutput<T> {
	/// Transform the decoded value, preserving the `degraded` flag.
	fn map<U>(self, f: impl FnOnce(T) -> U) -> CallRecordedOutput<U> {
		CallRecordedOutput { value: f(self.value), degraded: self.degraded }
	}
}

/// A transaction's trace in the `TraceV1` shape eth-rpc serves, or the runtime's report that it
/// could not produce one. Only V2 runtimes report `NotTraced`.
pub enum TraceEntry {
	Traced(TraceV1),
	NotTraced,
}

impl From<TraceEntryV1> for TraceEntry {
	fn from(entry: TraceEntryV1) -> Self {
		match entry {
			TraceEntryV1::Traced(trace) => Self::Traced(trace_v2_as_v1(trace)),
			TraceEntryV1::NotTraced => Self::NotTraced,
		}
	}
}

/// Renders a `TraceV2` in the V1 shape eth-rpc serves, dropping `CallLogV2::index`. Remove once
/// eth-rpc serves V2.
fn trace_v2_as_v1(trace: TraceV2) -> TraceV1 {
	match trace {
		TraceV2::Call(trace) => TraceV1::Call(call_trace_v2_as_v1(trace)),
		TraceV2::Prestate(trace) => TraceV1::Prestate(trace),
		TraceV2::Execution(trace) => TraceV1::Execution(trace),
	}
}

fn call_trace_v2_as_v1(trace: CallTraceV2) -> CallTraceV1 {
	CallTraceV1 {
		from: trace.from,
		gas: trace.gas,
		gas_used: trace.gas_used,
		to: trace.to,
		input: trace.input,
		output: trace.output,
		error: trace.error,
		revert_reason: trace.revert_reason,
		calls: trace.calls.into_iter().map(call_trace_v2_as_v1).collect(),
		logs: trace
			.logs
			.into_iter()
			.map(|log| CallLogV1 {
				address: log.address,
				topics: log.topics,
				data: log.data,
				position: log.position,
			})
			.collect(),
		value: trace.value,
		call_type: trace.call_type,
		// Tracer-internal, never serialized.
		child_call_count: 0,
	}
}

impl VersionAwareRuntimeApi {
	/// Create a new instance.
	pub fn new(
		at_block: OnlineClientAtBlock<SrcChainConfig>,
		capabilities: ReviveRuntimeApiCapabilities,
		rpc_client: RpcClient,
	) -> Self {
		Self { at_block, capabilities, rpc_client }
	}

	/// Get the balance of the given address.
	pub fn balance(&self, address: H160) -> Option<BoxFuture<'_, Result<U256, ClientError>>> {
		self.capabilities.balance.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().balance(address);
				self.call(payload).await.map(|balance| balance.0).map_err(Into::into)
			},
			|_| async move {
				let input = BalanceInputPayloadV1 { address };
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.balance_versioned(BalanceVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						BalanceOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.balance
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the contract storage for the given contract address and key.
	pub fn get_storage(
		&self,
		contract_address: H160,
		key: [u8; 32],
	) -> Option<BoxFuture<'_, Result<Option<Vec<u8>>, ClientError>>> {
		self.capabilities.get_storage.handle(
			|| async move {
				let payload =
					subxt_client::runtime_apis().revive_api().get_storage(contract_address, key);
				self.call(payload).await?.map_err(|_| ClientError::ContractNotFound)
			},
			|_| async move {
				let input = GetStorageInputPayloadV1 {
					address: contract_address,
					key: StorageKeyV1::Fixed(key),
				};
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.get_storage_versioned(GetStorageVersionedInputPayload::from(input).into());
				self.call(payload)
					.await?
					.map(|output| {
						GetStorageOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.storage
					})
					.map_err(|_| ClientError::ContractNotFound)
			},
		)
	}

	/// Estimates the minimum gas limit required for the transaction execution. Returns a [`U256`]
	/// of the gas limit.
	///
	/// Falls back to a [`dry_run`] of the transaction on runtimes which do not have an estimation
	/// runtime API function available.
	///
	/// [`dry_run`]: Self::dry_run
	pub fn estimate_gas(
		&self,
		tx: GenericTransactionV1,
		block: BlockId,
	) -> Option<BoxFuture<'_, Result<U256, ClientError>>> {
		let timestamp_override = block.is_pending().then(|| Timestamp::current().as_millis());

		self.capabilities
			.eth_estimate_gas
			.handle(
				|| {
					let tx = tx.clone();
					async move {
						let config = DryRunConfigV1 { timestamp_override, ..Default::default() };
						let payload = subxt_client::runtime_apis()
							.revive_api()
							.eth_estimate_gas(tx.into(), config.into());
						self.call(payload)
							.await?
							.map(|gas_estimate| gas_estimate.0)
							.map_err(|err| ClientError::TransactError(err.0))
					}
				},
				|_| {
					let tx = tx.clone();
					async move {
						let input = EstimateGasInputPayloadV1 {
							tx,
							timestamp_override,
							state_overrides: None,
						};
						let payload =
							subxt_client::runtime_apis().revive_api().eth_estimate_gas_versioned(
								EstimateGasVersionedInputPayload::from(input).into(),
							);
						self.call(payload)
							.await?
							.map(|output| {
								EstimateGasOutputPayloadV1::try_from(output.0)
									.expect("v1 input must produce v1 output; qed")
									.gas_estimate
							})
							.map_err(|err| ClientError::TransactError(err.0))
					}
				},
			)
			.or_else(|| {
				self.dry_run(tx.clone(), block, None)
					.map(|future| future.map_ok(|info| info.eth_gas).boxed())
			})
	}

	/// Dry run a transaction and return the [`EthTransactInfoV1`] for the transaction.
	///
	/// The runtime API functions are tried in order of expressiveness: the versioned
	/// `eth_transact`, then `eth_transact_with_config` (both carry the timestamp override and the
	/// state overrides), and only as a last resort the plain `eth_transact`, which carries
	/// neither and is therefore skipped whenever state overrides are requested.
	pub fn dry_run(
		&self,
		tx: GenericTransactionV1,
		block: BlockId,
		state_overrides: Option<StateOverrideSetV1>,
	) -> Option<BoxFuture<'_, Result<EthTransactInfoV1<Balance>, ClientError>>> {
		let timestamp_override = block.is_pending().then(|| Timestamp::current().as_millis());

		let versioned_eth_transact = match self.capabilities.eth_transact {
			Unavailable | Available(Unversioned) => None,
			Available(Versioned(_)) => {
				let tx = tx.clone();
				let state_overrides = state_overrides.clone();
				Some(
					async move {
						let input = TransactInputPayloadV1 {
							tx,
							timestamp_override,
							perform_balance_checks: true,
							state_overrides,
						};
						let payload =
							subxt_client::runtime_apis().revive_api().eth_transact_versioned(
								TransactVersionedInputPayload::from(input).into(),
							);
						self.call(payload)
							.await?
							.map(|output| {
								TransactOutputPayloadV1::try_from(output.0)
									.expect("v1 input must produce v1 output; qed")
									.transact_info
							})
							.map_err(|err| ClientError::TransactError(err.0))
					}
					.boxed(),
				)
			},
		};
		let eth_transact_with_config = match self.capabilities.eth_transact_with_config {
			Unavailable | Available(Versioned(_)) => None,
			Available(Unversioned) => {
				let tx = tx.clone();
				let state_overrides = state_overrides.clone();
				Some(
					async move {
						let config = DryRunConfigV1 {
							timestamp_override,
							state_overrides,
							..Default::default()
						};
						let payload = subxt_client::runtime_apis()
							.revive_api()
							.eth_transact_with_config(tx.into(), config.into());
						self.call(payload)
							.await?
							.map(|transact_info| transact_info.0)
							.map_err(|err| ClientError::TransactError(err.0))
					}
					.boxed(),
				)
			},
		};
		let plain_eth_transact = match self.capabilities.eth_transact {
			Unavailable | Available(Versioned(_)) => None,
			Available(Unversioned) if state_overrides.is_some() => None,
			Available(Unversioned) => {
				let tx = tx.clone();
				Some(
					async move {
						let payload =
							subxt_client::runtime_apis().revive_api().eth_transact(tx.into());
						self.call(payload)
							.await?
							.map(|transact_info| transact_info.0)
							.map_err(|err| ClientError::TransactError(err.0))
					}
					.boxed(),
				)
			},
		};

		versioned_eth_transact.or(eth_transact_with_config).or(plain_eth_transact)
	}

	/// Get the nonce of the given address.
	pub fn nonce(&self, address: H160) -> Option<BoxFuture<'_, Result<U256, ClientError>>> {
		self.capabilities.nonce.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().nonce(address);
				self.call(payload).await.map(|nonce| nonce.into()).map_err(Into::into)
			},
			|_| async move {
				let input = NonceInputPayloadV1 { address };
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.nonce_versioned(NonceVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						NonceOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.nonce
							.into()
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the gas price.
	pub fn gas_price(&self) -> Option<BoxFuture<'_, Result<U256, ClientError>>> {
		self.capabilities.gas_price.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().gas_price();
				self.call(payload).await.map(|gas_price| gas_price.0).map_err(Into::into)
			},
			|_| async move {
				let input = GasPriceInputPayloadV1;
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.gas_price_versioned(GasPriceVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						GasPriceOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.gas_price
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the block gas limit.
	pub fn block_gas_limit(&self) -> Option<BoxFuture<'_, Result<U256, ClientError>>> {
		self.capabilities.block_gas_limit.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().block_gas_limit();
				self.call(payload).await.map(|gas_limit| gas_limit.0).map_err(Into::into)
			},
			|_| async move {
				let input = BlockGasLimitInputPayloadV1;
				let payload = subxt_client::runtime_apis().revive_api().block_gas_limit_versioned(
					BlockGasLimitVersionedInputPayload::from(input).into(),
				);
				self.call(payload)
					.await
					.map(|output| {
						BlockGasLimitOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.block_gas_limit
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the miner address.
	pub fn block_author(&self) -> Option<BoxFuture<'_, Result<H160, ClientError>>> {
		self.capabilities.block_author.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().block_author();
				self.call(payload).await.map_err(Into::into)
			},
			|_| async move {
				let input = BlockAuthorInputPayloadV1;
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.block_author_versioned(BlockAuthorVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						BlockAuthorOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.block_author
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the trace for the given transaction index in the given block. `block_hash` is the traced
	/// block's on-chain hash (locates its proof-size recording).
	pub fn trace_tx(
		&self,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		transaction_index: u32,
		tracer_type: TracerTypeV1,
		block_hash: H256,
	) -> Option<BoxFuture<'_, Result<CallRecordedOutput<Option<TraceEntry>>, ClientError>>> {
		match self.capabilities.trace_tx {
			Unavailable => None,
			Available(Unversioned) => {
				let future = Box::pin(async move {
					let payload = subxt_client::runtime_apis().revive_api().trace_tx(
						block.into(),
						transaction_index,
						tracer_type.into(),
					);
					let output = self.call_recorded_with_fallback(payload, block_hash).await?;
					Ok(output.map(|value| value.map(|trace| TraceEntry::Traced(trace.0))))
				});
				Some(future)
			},
			Available(Versioned(0..2)) => {
				let future = Box::pin(async move {
					let input = TraceTxInputPayloadV1 {
						block: block.into(),
						tx_index: transaction_index,
						config: tracer_type,
					};
					let payload = subxt_client::runtime_apis()
						.revive_api()
						.trace_tx_versioned(TraceTxVersionedInputPayload::from(input).into());
					let output = self.call_recorded_with_fallback(payload, block_hash).await?;
					Ok(output.map(|value| {
						TraceTxOutputPayloadV1::try_from(value.0)
							.expect("v1 input must produce v1 output; qed")
							.trace
							.map(TraceEntry::Traced)
					}))
				});
				Some(future)
			},
			Available(Versioned(2..)) => {
				let future = Box::pin(async move {
					let input = TraceTxInputPayloadV2 {
						block: block.into(),
						tx_index: transaction_index,
						config: tracer_type,
					};
					let payload = subxt_client::runtime_apis()
						.revive_api()
						.trace_tx_versioned(TraceTxVersionedInputPayload::from(input).into());
					let output = self.call_recorded_with_fallback(payload, block_hash).await?;
					let entry = TraceTxOutputPayloadV2::try_from(output.value.0)
						.expect("v2 input must produce v2 output; qed")
						.entry
						.map(TraceEntry::from);
					Ok(CallRecordedOutput { value: entry, degraded: false })
				});
				Some(future)
			},
		}
	}

	/// Get the trace for the given block. `block_hash` is the traced block's on-chain hash (locates
	/// its proof-size recording).
	pub fn trace_block(
		&self,
		block: sp_runtime::generic::Block<
			sp_runtime::generic::Header<u32, sp_runtime::traits::BlakeTwo256>,
			sp_runtime::OpaqueExtrinsic,
		>,
		tracer_type: TracerTypeV1,
		block_hash: H256,
	) -> Option<BoxFuture<'_, Result<CallRecordedOutput<Vec<(u32, TraceEntry)>>, ClientError>>> {
		match self.capabilities.trace_block {
			Unavailable => None,
			Available(Unversioned) => {
				let future = Box::pin(async move {
					let payload = subxt_client::runtime_apis()
						.revive_api()
						.trace_block(block.into(), tracer_type.into());
					let output = self.call_recorded_with_fallback(payload, block_hash).await?;
					Ok(output.map(|traces| {
						traces
							.into_iter()
							.map(|(idx, trace)| (idx, TraceEntry::Traced(trace.0)))
							.collect()
					}))
				});
				Some(future)
			},
			Available(Versioned(0..2)) => {
				let future = Box::pin(async move {
					let input =
						TraceBlockInputPayloadV1 { block: block.into(), config: tracer_type };
					let payload = subxt_client::runtime_apis()
						.revive_api()
						.trace_block_versioned(TraceBlockVersionedInputPayload::from(input).into());
					let output = self.call_recorded_with_fallback(payload, block_hash).await?;
					Ok(output.map(|value| {
						TraceBlockOutputPayloadV1::try_from(value.0)
							.expect("v1 input must produce v1 output; qed")
							.traces
							.into_iter()
							.map(|(idx, trace)| (idx, TraceEntry::Traced(trace)))
							.collect()
					}))
				});
				Some(future)
			},
			Available(Versioned(2..)) => {
				let future = Box::pin(async move {
					let input =
						TraceBlockInputPayloadV2 { block: block.into(), config: tracer_type };
					let payload = subxt_client::runtime_apis()
						.revive_api()
						.trace_block_versioned(TraceBlockVersionedInputPayload::from(input).into());
					let output = self.call_recorded_with_fallback(payload, block_hash).await?;
					let entries = TraceBlockOutputPayloadV2::try_from(output.value.0)
						.expect("v2 input must produce v2 output; qed")
						.entries
						.into_iter()
						.map(|(idx, entry)| (idx, entry.into()))
						.collect();
					Ok(CallRecordedOutput { value: entries, degraded: false })
				});
				Some(future)
			},
		}
	}

	/// Get the trace for the given call.
	pub fn trace_call(
		&self,
		transaction: GenericTransactionV1,
		tracer_type: TracerTypeV1,
		state_overrides: Option<StateOverrideSetV1>,
	) -> Option<BoxFuture<'_, Result<TraceV1, ClientError>>> {
		let trace_call_future =
			self.capabilities.trace_call.handle::<'_, _, _, _, BoxFuture<'_, _>, _>(
				|| {
					if state_overrides.is_some() {
						return None;
					};

					let transaction = transaction.clone();
					let tracer_type = tracer_type.clone();
					Some(Box::pin(async move {
						let payload = subxt_client::runtime_apis()
							.revive_api()
							.trace_call(transaction.into(), tracer_type.into());
						self.call(payload)
							.await?
							.map(|trace| trace.0)
							.map_err(|err| ClientError::TransactError(err.0))
					}))
				},
				|_| {
					let transaction = transaction.clone();
					let tracer_type = tracer_type.clone();
					let state_overrides = state_overrides.clone();
					async move {
						let input = TraceCallInputPayloadV1 {
							tx: transaction,
							config: tracer_type,
							state_overrides,
						};
						let payload =
							subxt_client::runtime_apis().revive_api().trace_call_versioned(
								TraceCallVersionedInputPayload::from(input).into(),
							);
						self.call(payload)
							.await?
							.map(|output| {
								TraceCallOutputPayloadV1::try_from(output.0)
									.expect("v1 input must produce v1 output; qed")
									.trace
							})
							.map_err(|err| ClientError::TransactError(err.0))
					}
				},
			);
		let trace_call_with_config_future = self
			.capabilities
			.trace_call_with_config
			.handle::<'_, _, _, _, _, BoxFuture<'_, _>>(
				|| {
					let transaction = transaction.clone();
					let tracer_type = tracer_type.clone();
					let state_overrides = state_overrides.clone();
					async move {
						let config = TracingConfigV1 { state_overrides };
						let payload =
							subxt_client::runtime_apis().revive_api().trace_call_with_config(
								transaction.into(),
								tracer_type.into(),
								config.into(),
							);
						self.call(payload)
							.await?
							.map(|trace| trace.0)
							.map_err(|err| ClientError::TransactError(err.0))
					}
				},
				|_| None::<BoxFuture<'_, _>>,
			);

		trace_call_future.or(trace_call_with_config_future)
	}

	/// Get the code of the given address.
	pub fn code(&self, address: H160) -> Option<BoxFuture<'_, Result<Vec<u8>, ClientError>>> {
		self.capabilities.code.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().code(address);
				self.call(payload).await.map_err(Into::into)
			},
			|_| async move {
				let input = CodeInputPayloadV1 { address };
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.code_versioned(CodeVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						CodeOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.code
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the current Ethereum block.
	pub fn eth_block(&self) -> Option<BoxFuture<'_, Result<BlockV1, ClientError>>> {
		self.capabilities.eth_block.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().eth_block();
				self.call(payload).await.map(|block| block.0).map_err(Into::into)
			},
			|_| async move {
				let input = BlockInputPayloadV1;
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.eth_block_versioned(BlockVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						BlockOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.block
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the Ethereum block hash for the given block number.
	pub fn eth_block_hash(
		&self,
		number: U256,
	) -> Option<BoxFuture<'_, Result<Option<H256>, ClientError>>> {
		self.capabilities.eth_block_hash.handle(
			|| async move {
				let payload =
					subxt_client::runtime_apis().revive_api().eth_block_hash(number.into());
				self.call(payload).await.map_err(Into::into)
			},
			|_| async move {
				let input = BlockHashInputPayloadV1 { block_number: number };
				let payload = subxt_client::runtime_apis()
					.revive_api()
					.eth_block_hash_versioned(BlockHashVersionedInputPayload::from(input).into());
				self.call(payload)
					.await
					.map(|output| {
						BlockHashOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.block_hash
					})
					.map_err(Into::into)
			},
		)
	}

	/// Get the receipt data for the current block.
	pub fn eth_receipt_data(
		&self,
	) -> Option<BoxFuture<'_, Result<Vec<ReceiptGasInfoV1>, ClientError>>> {
		self.capabilities.eth_receipt_data.handle(
			|| async move {
				let payload = subxt_client::runtime_apis().revive_api().eth_receipt_data();
				self.call(payload)
					.await
					.map(|receipt_data| receipt_data.into_iter().map(|item| item.0).collect())
					.map_err(Into::into)
			},
			|_| async move {
				let input = ReceiptDataInputPayloadV1;
				let payload = subxt_client::runtime_apis().revive_api().eth_receipt_data_versioned(
					ReceiptDataVersionedInputPayload::from(input).into(),
				);
				self.call(payload)
					.await
					.map(|output| {
						ReceiptDataOutputPayloadV1::try_from(output.0)
							.expect("v1 input must produce v1 output; qed")
							.receipt_data
					})
					.map_err(Into::into)
			},
		)
	}

	/// The primary way of calling the runtime API throughout the eth-rpc.
	///
	/// Every call made through a constructed `VersionAwareRuntimeApi` uses this final dispatch
	/// point. It applies [`StaticPayload::unvalidated`] centrally because the eth-rpc handles
	/// runtime compatibility itself instead of relying on Subxt's static validation.
	///
	/// Capability discovery performs one direct bootstrap call before a `VersionAwareRuntimeApi`
	/// can be constructed.
	async fn call<ArgsType, ReturnType>(
		&self,
		payload: StaticPayload<ArgsType, ReturnType>,
	) -> Result<ReturnType, RuntimeApiError>
	where
		StaticPayload<ArgsType, ReturnType>: Payload<ArgsType = ArgsType, ReturnType = ReturnType>,
	{
		let payload = payload.unvalidated();
		self.at_block.runtime_apis().call(payload).await
	}

	/// Run `payload` via the node's `state_callRecorded` RPC, re-enacting `block` with a proof-size
	/// recorder. If the node cannot service it (method absent/denied, or no recorder), retry the
	/// same payload through [`call`](Self::call), setting [`CallRecordedOutput::degraded`] when
	/// that fallback may have dropped traces. Other errors propagate.
	async fn call_recorded_with_fallback<ArgsType, ReturnType>(
		&self,
		payload: StaticPayload<ArgsType, ReturnType>,
		block: H256,
	) -> Result<CallRecordedOutput<ReturnType>, ClientError>
	where
		StaticPayload<ArgsType, ReturnType>: Payload<ArgsType = ArgsType, ReturnType = ReturnType>,
		ReturnType: IntoVisitor,
	{
		let runtime_apis = self.at_block.runtime_apis();
		let name = runtime_apis.encode_name(&payload);
		let args = runtime_apis.encode_args(&payload).map_err(ClientError::from)?;

		let recorded = match self
			.rpc_client
			.request::<Bytes>("state_callRecorded", rpc_params![name, Bytes(args), block])
			.await
		{
			Ok(bytes) => {
				let metadata = self.at_block.metadata_ref();
				let cursor = &mut &bytes.0[..];
				frame_decode::runtime_apis::decode_runtime_api_response(
					payload.trait_name(),
					payload.method_name(),
					cursor,
					metadata,
					metadata.types(),
					ReturnType::into_visitor(),
				)
				.map_err(RuntimeApiError::CouldNotDecodeResponse)
				.map_err(ClientError::from)
			},
			Err(err) => Err(ClientError::from(err)),
		};

		let err = match recorded {
			Ok(value) => return Ok(CallRecordedOutput { value, degraded: false }),
			Err(err) => err,
		};
		let Some(reason) = err.recorded_unavailable_reason() else { return Err(err) };
		match reason {
			RecordedUnavailable::MethodMissing => log::warn!(
				target: LOG_TARGET,
				"node does not expose `state_callRecorded` (predates it — upgrade the node); \
				 falling back to recorder-less replay — traces may be INCOMPLETE on PoV/parachain \
				 chains",
			),
			RecordedUnavailable::Denied => log::warn!(
				target: LOG_TARGET,
				"`state_callRecorded` denied (unsafe RPC methods disabled — enable them); falling \
				 back to recorder-less replay — traces may be INCOMPLETE on PoV/parachain chains",
			),
			RecordedUnavailable::NoRecorder => log::debug!(
				target: LOG_TARGET,
				"node registers no proof-size recorder; using plain replay (correct, no reclaim \
				 to honour)",
			),
		}
		let value = self.call(payload).await.map_err(ClientError::from)?;
		Ok(CallRecordedOutput { value, degraded: reason.is_degraded() })
	}
}

/// Hands out [`VersionAwareRuntimeApi`] instances for specific blocks, caching the
/// [`ReviveRuntimeApiCapabilities`] that each one is constructed with.
///
/// Capabilities are cached by runtime spec version, which changes whenever runtime behavior
/// changes. Subxt resolves the spec version and its metadata while constructing an
/// [`OnlineClientAtBlock`] and uses the same key for its own metadata cache. Consequently, blocks
/// running the same runtime share one capability entry without requiring block-by-block
/// observation or runtime-upgrade detection.
///
/// [`at`]: Self::at
#[derive(Clone)]
pub struct VersionAwareRuntimeApiProvider {
	/// The subxt client through which the capabilities of blocks are computed.
	api: OnlineClient<SrcChainConfig>,
	rpc_client: RpcClient,
	/// The capabilities of each encountered runtime spec version.
	cache: Arc<Mutex<HashMap<u32, ReviveRuntimeApiCapabilities>>>,
}

impl VersionAwareRuntimeApiProvider {
	/// Creates a provider with an empty cache which computes capabilities through the given
	/// client.
	pub fn new(api: OnlineClient<SrcChainConfig>, rpc_client: RpcClient) -> Self {
		Self { api, rpc_client, cache: Arc::new(Mutex::new(HashMap::new())) }
	}

	/// Returns the version-aware runtime API of the given block, computing and caching the
	/// capabilities of its runtime spec version if they are not cached yet.
	pub async fn at(&self, block_hash: H256) -> Result<VersionAwareRuntimeApi, ClientError> {
		let at_block = self.api.at_block(block_hash).await?;
		let capabilities = self.capabilities(&at_block).await?;
		Ok(VersionAwareRuntimeApi::new(at_block, capabilities, self.rpc_client.clone()))
	}

	/// Returns the version-aware runtime API for a block when both its Substrate hash and number
	/// are already known.
	///
	/// Supplying both values avoids the header request that [`Self::at`] needs to derive the block
	/// number from its hash. The number is the Substrate height, even where the caller also uses it
	/// as an Ethereum block number; pallet-revive defines those heights to be identical.
	///
	/// # Warning
	///
	/// `block_number` must identify `block_hash`. A mismatch can make Subxt use runtime metadata
	/// from a different block than the state addressed by the hash.
	pub async fn at_block_hash_and_number(
		&self,
		block_hash: H256,
		block_number: SubstrateBlockNumber,
	) -> Result<VersionAwareRuntimeApi, ClientError> {
		let at_block = self.api.at_block_hash_and_number(block_hash, block_number).await?;
		let capabilities = self.capabilities(&at_block).await?;
		Ok(VersionAwareRuntimeApi::new(at_block, capabilities, self.rpc_client.clone()))
	}

	/// Returns the version-aware runtime API of the given block handle.
	pub async fn at_resolved_block(
		&self,
		at_block: OnlineClientAtBlock<SrcChainConfig>,
	) -> Result<VersionAwareRuntimeApi, ClientError> {
		let capabilities = self.capabilities(&at_block).await?;
		Ok(VersionAwareRuntimeApi::new(at_block, capabilities, self.rpc_client.clone()))
	}

	/// Returns the capabilities of the handle's runtime spec version, computing and caching them
	/// if they are not cached yet.
	async fn capabilities(
		&self,
		at_block: &OnlineClientAtBlock<SrcChainConfig>,
	) -> Result<ReviveRuntimeApiCapabilities, ClientError> {
		let spec_version = at_block.spec_version();
		if let Some(capabilities) = self.lock_cache().get(&spec_version).copied() {
			return Ok(capabilities);
		}

		let capabilities =
			ReviveRuntimeApiCapabilities::new(at_block.metadata_ref(), at_block).await?;
		self.lock_cache().insert(spec_version, capabilities);
		log::debug!(target: LOG_TARGET,
			"Computed the revive runtime API capabilities of spec version {spec_version}");
		Ok(capabilities)
	}

	/// Locks the cache, tolerating lock poisoning since the cache holds no invariants beyond the
	/// cached entries themselves.
	fn lock_cache(&self) -> MutexGuard<'_, HashMap<u32, ReviveRuntimeApiCapabilities>> {
		self.cache.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// Stores the capabilities of pallet-revive's runtime API.
///
/// New methods were added to pallet-revive over time without making proper use of frame's API
/// version. Therefore, there is no clean mapping of "in API version X, function A was added" or
/// anything of this sort. All such information needs to be obtained by analyzing the metadata at
/// a particular block to deduce such information.
///
/// This structure provides precisely this information and is used to answer the question "is method
/// X available on the runtime API for this block or not".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ReviveRuntimeApiCapabilities {
	pub eth_block: MethodStatus,
	pub eth_block_hash: MethodStatus,
	pub eth_receipt_data: MethodStatus,
	pub block_gas_limit: MethodStatus,
	pub max_extrinsic_weight_in_gas: MethodStatus,
	pub balance: MethodStatus,
	pub gas_price: MethodStatus,
	pub nonce: MethodStatus,
	pub call: MethodStatus,
	pub instantiate: MethodStatus,
	pub eth_transact: MethodStatus,
	pub eth_transact_with_config: MethodStatus,
	pub eth_estimate_gas: MethodStatus,
	pub eth_pre_dispatch_weight: MethodStatus,
	pub upload_code: MethodStatus,
	pub get_storage: MethodStatus,
	pub runtime_pallets_address: MethodStatus,
	pub code: MethodStatus,
	pub account_id: MethodStatus,
	pub new_balance_with_dust: MethodStatus,
	pub block_author: MethodStatus,
	pub address: MethodStatus,
	pub trace_block: MethodStatus,
	pub trace_tx: MethodStatus,
	pub trace_call: MethodStatus,
	pub trace_call_with_config: MethodStatus,
}

impl ReviveRuntimeApiCapabilities {
	/// Constructs the revive runtime API capabilities.
	///
	/// This requires the metadata and the runtime API at the same block in order to be constructed.
	/// If they come from different blocks then the object created might end up with corrupted state
	/// that is not representative of any real block on the network.
	///
	/// Subxt normalizes every metadata version it supports into [`Metadata`]. If that metadata does
	/// not describe pallet-revive's runtime API, no method is reported as available.
	///
	/// If metadata declares `version_declarations`, any failure to call it is returned instead of
	/// caching incomplete capabilities.
	pub async fn new(
		metadata: &Metadata,
		at_block: &OnlineClientAtBlock<SrcChainConfig>,
	) -> Result<Self, ClientError> {
		let version_declarations = match metadata
			.runtime_api_trait_by_name("ReviveApi")
			.and_then(|api| api.method_by_name("version_declarations"))
		{
			Some(_) => Some(
				at_block
					.runtime_apis()
					.call(
						subxt_client::runtime_apis()
							.revive_api()
							.version_declarations()
							.unvalidated(),
					)
					.await?,
			),
			None => None,
		};

		let metadata_methods = metadata
			.runtime_api_trait_by_name("ReviveApi")
			.into_iter()
			.flat_map(|api| api.methods())
			.map(|method| method.name());

		Ok(version_declarations
			.iter()
			.flat_map(|declarations| declarations.0.iter())
			.map(|(method, version)| (method, Versioned(*version)))
			.chain(metadata_methods.map(|method| (method, Unversioned)))
			.fold(Self::default(), |this, (method, versioning_status)| {
				this.with_method(method, versioning_status)
			}))
	}

	fn with_method(
		mut self,
		key: impl AsRef<str>,
		incoming_method_status: MethodVersioningStatus,
	) -> Self {
		let key = key.as_ref();
		let key = key.strip_suffix("_versioned").unwrap_or(key);
		let existing_method_status = match key {
			"eth_block" => Some(&mut self.eth_block),
			"eth_block_hash" => Some(&mut self.eth_block_hash),
			"eth_receipt_data" => Some(&mut self.eth_receipt_data),
			"block_gas_limit" => Some(&mut self.block_gas_limit),
			"max_extrinsic_weight_in_gas" => Some(&mut self.max_extrinsic_weight_in_gas),
			"balance" => Some(&mut self.balance),
			"gas_price" => Some(&mut self.gas_price),
			"nonce" => Some(&mut self.nonce),
			"call" => Some(&mut self.call),
			"instantiate" => Some(&mut self.instantiate),
			"eth_transact" => Some(&mut self.eth_transact),
			"eth_transact_with_config" => Some(&mut self.eth_transact_with_config),
			"eth_estimate_gas" => Some(&mut self.eth_estimate_gas),
			"eth_pre_dispatch_weight" => Some(&mut self.eth_pre_dispatch_weight),
			"upload_code" => Some(&mut self.upload_code),
			"get_storage" => Some(&mut self.get_storage),
			"runtime_pallets_address" => Some(&mut self.runtime_pallets_address),
			"code" => Some(&mut self.code),
			"account_id" => Some(&mut self.account_id),
			"new_balance_with_dust" => Some(&mut self.new_balance_with_dust),
			"block_author" => Some(&mut self.block_author),
			"address" => Some(&mut self.address),
			"trace_block" => Some(&mut self.trace_block),
			"trace_tx" => Some(&mut self.trace_tx),
			"trace_call" => Some(&mut self.trace_call),
			"trace_call_with_config" => Some(&mut self.trace_call_with_config),
			_ => None,
		};
		let Some(existing_method_status) = existing_method_status else { return self };

		match (existing_method_status, incoming_method_status) {
			(this @ (Unavailable | Available(Unversioned)), Unversioned | Versioned(_)) => {
				*this = Available(incoming_method_status)
			},
			(Available(Versioned(existing_version)), Versioned(incoming_version)) => {
				*existing_version = (*existing_version).max(incoming_version)
			},
			(Available(Versioned(..)), Unversioned) => {},
		}

		self
	}
}

/// Defines the status of a runtime API function in pallet-revive and whether it's available or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum MethodStatus {
	/// The runtime API method is not available on the runtime API of the selected block (e.g., the
	/// `estimate_gas` runtime API function which was added later on).
	#[default]
	Unavailable,

	/// The runtime API method is available on the runtime API, and may either be versioned or
	/// unversioned.
	Available(MethodVersioningStatus),
}

impl MethodStatus {
	/// Selects and boxes the future produced by the handler matching this status.
	fn handle<'a, T, Rtn1, Rtn2, Fut1, Fut2>(
		&'a self,
		unversioned_handler: impl FnOnce() -> Rtn1,
		versioned_handler: impl FnOnce(u8) -> Rtn2,
	) -> Option<BoxFuture<'a, T>>
	where
		Rtn1: Into<Option<Fut1>>,
		Rtn2: Into<Option<Fut2>>,
		Fut1: Future<Output = T> + Send + 'a,
		Fut2: Future<Output = T> + Send + 'a,
	{
		match *self {
			Unavailable => None,
			Available(Unversioned) => unversioned_handler().into().map(|fut| fut.boxed()),
			Available(Versioned(v)) => versioned_handler(v).into().map(|fut| fut.boxed()),
		}
	}
}

/// Defines the status of a runtime API function's versioning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MethodVersioningStatus {
	/// The method is available on the runtime API and is not versioned (e.g., the specified block
	/// has a pre-versioning runtime).
	Unversioned,

	/// The method is available on the runtime API and is versioned. The provided value is the
	/// highest version supported by the runtime.
	Versioned(u8),
}

#[cfg(test)]
mod tests {
	use super::*;
	use codec::Decode;

	/// Ensures every known method is mapped and version precedence is preserved.
	#[test]
	fn maps_known_methods_and_preserves_version_precedence() {
		// Arrange
		let expected = ReviveRuntimeApiCapabilities {
			eth_block: Available(Versioned(4)),
			eth_block_hash: Available(Unversioned),
			eth_receipt_data: Available(Versioned(1)),
			block_gas_limit: Available(Unversioned),
			max_extrinsic_weight_in_gas: Available(Versioned(2)),
			balance: Available(Unversioned),
			gas_price: Available(Versioned(3)),
			nonce: Available(Unversioned),
			call: Available(Versioned(4)),
			instantiate: Available(Unversioned),
			eth_transact: Available(Versioned(5)),
			eth_transact_with_config: Available(Unversioned),
			eth_estimate_gas: Available(Versioned(6)),
			eth_pre_dispatch_weight: Available(Unversioned),
			upload_code: Available(Versioned(7)),
			get_storage: Available(Unversioned),
			runtime_pallets_address: Available(Versioned(8)),
			code: Available(Unversioned),
			account_id: Available(Versioned(9)),
			new_balance_with_dust: Available(Unversioned),
			block_author: Available(Versioned(10)),
			address: Available(Unversioned),
			trace_block: Available(Versioned(11)),
			trace_tx: Available(Unversioned),
			trace_call: Available(Versioned(12)),
			trace_call_with_config: Available(Unversioned),
		};

		// Act
		let actual = ReviveRuntimeApiCapabilities::default()
			.with_method("eth_block", Unversioned)
			.with_method("eth_block_versioned", Versioned(2))
			.with_method("eth_block_versioned", Versioned(4))
			.with_method("eth_block_versioned", Versioned(3))
			.with_method("eth_block", Unversioned)
			.with_method("eth_block_hash", Unversioned)
			.with_method("eth_receipt_data_versioned", Versioned(1))
			.with_method("block_gas_limit", Unversioned)
			.with_method("max_extrinsic_weight_in_gas_versioned", Versioned(2))
			.with_method("balance", Unversioned)
			.with_method("gas_price_versioned", Versioned(3))
			.with_method("nonce", Unversioned)
			.with_method("call_versioned", Versioned(4))
			.with_method("instantiate", Unversioned)
			.with_method("eth_transact_versioned", Versioned(5))
			.with_method("eth_transact_with_config", Unversioned)
			.with_method("eth_estimate_gas_versioned", Versioned(6))
			.with_method("eth_pre_dispatch_weight", Unversioned)
			.with_method("upload_code_versioned", Versioned(7))
			.with_method("get_storage", Unversioned)
			.with_method("runtime_pallets_address_versioned", Versioned(8))
			.with_method("code", Unversioned)
			.with_method("account_id_versioned", Versioned(9))
			.with_method("new_balance_with_dust", Unversioned)
			.with_method("block_author_versioned", Versioned(10))
			.with_method("address", Unversioned)
			.with_method("trace_block_versioned", Versioned(11))
			.with_method("trace_tx", Unversioned)
			.with_method("trace_call_versioned", Versioned(12))
			.with_method("trace_call_with_config", Unversioned)
			.with_method("unknown_method_versioned", Versioned(u8::MAX));

		// Assert
		assert_eq!(actual, expected);
	}

	/// Ensures every supported method in the generated metadata is mapped.
	#[test]
	fn every_supported_metadata_method_updates_capabilities() {
		// Arrange
		let metadata_bytes: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/revive_chain.scale"));
		let metadata = Metadata::decode(&mut &metadata_bytes[..]).unwrap();
		let revive_api = metadata.runtime_api_trait_by_name("ReviveApi").unwrap();
		let methods = revive_api.methods().filter(|method| {
			method.name() != "version_declarations" && method.name() != "get_storage_var_key"
		});

		// Act
		for method in methods {
			let before = ReviveRuntimeApiCapabilities::default();
			let after = before.with_method(method.name(), Unversioned);

			// Assert
			assert_ne!(before, after, "`{}` is not mapped by `with_method`", method.name());
		}
	}
}
