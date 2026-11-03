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
use crate::*;
use jsonrpsee::{core::RpcResult, proc_macros::rpc};

/// Debug Ethererum JSON-RPC apis.
#[rpc(server, client)]
pub trait DebugRpc {
	/// Returns the tracing of the execution of a specific block using its number.
	///
	/// ## References
	///
	/// - <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-debug#debugtraceblockbynumber>
	#[method(name = "debug_traceBlockByNumber")]
	async fn trace_block_by_number(
		&self,
		block: BlockNumberOrTag,
		tracer_config: Option<TracerConfig>,
	) -> RpcResult<Vec<TransactionTrace>>;

	/// Returns a transaction's traces by replaying it.
	///
	/// ## References
	///
	/// - <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-debug#debugtracetransaction>
	#[method(name = "debug_traceTransaction")]
	async fn trace_transaction(
		&self,
		transaction_hash: H256,
		tracer_config: Option<TracerConfig>,
	) -> RpcResult<TraceV1>;

	/// Dry run a call and returns the transaction's traces.
	///
	/// Accepts an optional [`TraceCallConfig`] that extends the base tracer config with state
	/// overrides, matching the [Geth specification](https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-debug#debugtracecall).
	///
	/// ## References
	///
	/// - <https://geth.ethereum.org/docs/interacting-with-geth/rpc/ns-debug#debugtracecall>
	#[method(name = "debug_traceCall")]
	async fn trace_call(
		&self,
		transaction: GenericTransactionV1,
		block: BlockId,
		trace_call_config: Option<TraceCallConfig>,
	) -> RpcResult<TraceV1>;

	#[method(name = "debug_getAutomine")]
	async fn get_automine(&self) -> RpcResult<bool>;
}

pub struct DebugRpcServerImpl {
	client: client::Client,
}

impl DebugRpcServerImpl {
	pub fn new(client: client::Client) -> Self {
		Self { client }
	}
}

async fn with_timeout<T>(
	timeout: Option<core::time::Duration>,
	fut: impl std::future::Future<Output = Result<T, ClientError>>,
) -> RpcResult<T> {
	if let Some(timeout) = timeout {
		match tokio::time::timeout(timeout, fut).await {
			Ok(r) => Ok(r?),
			Err(_) => Err(ErrorObjectOwned::owned::<String>(
				-32000,
				"execution timeout".to_string(),
				None,
			)),
		}
	} else {
		Ok(fut.await?)
	}
}

#[async_trait]
impl DebugRpcServer for DebugRpcServerImpl {
	async fn trace_block_by_number(
		&self,
		block: BlockNumberOrTag,
		tracer_config: Option<TracerConfig>,
	) -> RpcResult<Vec<TransactionTrace>> {
		let TracerConfig { config, timeout } = tracer_config.unwrap_or_default();
		with_timeout(timeout, self.client.trace_block_by_number(block, config)).await
	}

	async fn trace_transaction(
		&self,
		transaction_hash: H256,
		tracer_config: Option<TracerConfig>,
	) -> RpcResult<TraceV1> {
		let TracerConfig { config, timeout } = tracer_config.unwrap_or_default();
		with_timeout(timeout, self.client.trace_transaction(transaction_hash, config)).await
	}

	async fn trace_call(
		&self,
		transaction: GenericTransactionV1,
		block: BlockId,
		trace_call_config: Option<TraceCallConfig>,
	) -> RpcResult<TraceV1> {
		let TraceCallConfig { tracer_config, state_overrides } =
			trace_call_config.unwrap_or_default();
		let TracerConfig { config, timeout } = tracer_config;
		with_timeout(timeout, self.client.trace_call(transaction, block, config, state_overrides))
			.await
	}

	async fn get_automine(&self) -> RpcResult<bool> {
		sc_service::Result::Ok(self.client.get_automine().await)
	}
}
