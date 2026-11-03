// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.
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

use cumulus_client_consensus_common::ParachainBlockImport as TParachainBlockImport;
use cumulus_primitives_core::relay_chain::UncheckedExtrinsic;
use sc_consensus::DefaultImportQueue;
use sc_executor::WasmExecutor;
use sc_service::{PartialComponents, TFullBackend, TFullClient};
use sc_telemetry::{Telemetry, TelemetryWorkerHandle};
use sc_transaction_pool::TransactionPoolHandle;
use sp_runtime::{generic, traits::BlakeTwo256};

pub use parachains_common_types::{AccountId, Balance, Hash, Nonce};

type Header<BlockNumber> = generic::Header<BlockNumber, BlakeTwo256>;
pub type Block<BlockNumber> = generic::Block<Header<BlockNumber>, UncheckedExtrinsic>;

#[cfg(not(feature = "runtime-benchmarks"))]
pub type ParachainHostFunctions = (
	cumulus_client_service::ParachainHostFunctions,
	sp_statement_store::runtime_api::HostFunctions,
);
#[cfg(feature = "runtime-benchmarks")]
pub type ParachainHostFunctions = (
	cumulus_client_service::ParachainHostFunctions,
	sp_statement_store::runtime_api::HostFunctions,
	frame_benchmarking::benchmarking::HostFunctions,
);

pub type ParachainClient<Block, RuntimeApi> =
	TFullClient<Block, RuntimeApi, WasmExecutor<ParachainHostFunctions>>;

pub type ParachainBackend<Block> = TFullBackend<Block>;

pub type ParachainBlockImport<Block, BI> =
	TParachainBlockImport<Block, BI, ParachainBackend<Block>>;

/// Assembly of PartialComponents (enough to run chain ops subcommands)
pub type ParachainService<Block, RuntimeApi, BI, BIExtraReturnValue> = PartialComponents<
	ParachainClient<Block, RuntimeApi>,
	ParachainBackend<Block>,
	(),
	DefaultImportQueue<Block>,
	TransactionPoolHandle<Block, ParachainClient<Block, RuntimeApi>>,
	(
		ParachainBlockImport<Block, BI>,
		Option<Telemetry>,
		Option<TelemetryWorkerHandle>,
		BIExtraReturnValue,
		sc_storage_chain_sync::BitswapHandleSlot,
	),
>;
